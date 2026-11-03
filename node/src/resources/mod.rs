//! Node-local admission. A reservation is held by its operation and inherited
//! file locks in its VMMs, including across daemon restarts.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(test)]
mod tests;

use std::{
	collections::HashMap,
	fs::{self, File, OpenOptions},
	io,
	os::fd::AsRawFd,
	path::{Path, PathBuf},
	sync::Arc,
};

use anyhow::{Context, bail, ensure};
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use vmon_cluster::ResourceCapacity;

pub const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ResourcePolicy {
	#[default]
	Legacy,
	/// Admission plus a delegated Linux cgroup hierarchy; fails closed.
	Strict,
	/// Portable admission, without a kernel CPU/RAM usage ceiling.
	Reserved,
}

#[derive(Clone, Debug, Default)]
pub struct ResourceConfig {
	pub policy:          ResourcePolicy,
	pub vcpus:           u32,
	pub memory_mb:       u64,
	pub overhead_mb:     u64,
	pub host_reserve_mb: u64,
	pub hysteresis_mb:   u64,
	pub cgroup:          Option<PathBuf>,
}

impl ResourceConfig {
	pub fn validate(&self) -> anyhow::Result<()> {
		if self.policy == ResourcePolicy::Legacy {
			ensure!(
				self.vcpus == 0 && self.memory_mb == 0 && self.cgroup.is_none(),
				"resource limits require --resource-policy strict or reserved"
			);
			return Ok(());
		}
		ensure!(
			self.vcpus > 0 && self.memory_mb > self.overhead_mb,
			"resource budget must provide vCPUs and guest memory after overhead"
		);
		ensure!(self.overhead_mb > 0, "a positive VMM overhead allowance is required");
		ensure!(
			self.vcpus <= crate::host::cpus(),
			"vCPU budget exceeds the host's effective CPU capacity"
		);
		for value in [self.memory_mb, self.overhead_mb, self.host_reserve_mb, self.hysteresis_mb] {
			value
				.checked_mul(MIB)
				.context("resource budget overflows bytes")?;
		}
		ensure!(
			self.cgroup.is_none() || self.policy == ResourcePolicy::Strict,
			"--resource-cgroup requires strict policy"
		);
		Ok(())
	}
}

#[derive(Debug, thiserror::Error)]
#[error("node capacity unavailable: {0}")]
pub struct CapacityError(pub String);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Record {
	name:        String,
	vcpus:       u32,
	memory_mb:   u64,
	overhead_mb: u64,
}

#[derive(Default)]
struct State {
	records:  HashMap<String, Record>,
	pressure: bool,
}

struct Inner {
	config:      ResourceConfig,
	dir:         PathBuf,
	state:       Mutex<State>,
	backend:     platform::Backend,
	_owner:      File,
	_host_owner: File,
}

#[derive(Clone, Default)]
pub struct Resources(Option<Arc<Inner>>);

impl std::fmt::Debug for Resources {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Resources")
			.field("bounded", &self.bounded())
			.finish()
	}
}

#[derive(Clone, Default)]
pub struct Reservation(Option<Arc<Lease>>);

struct Lease {
	resources: Resources,
	id:        String,
	_lock:     File,
}

tokio::task_local! {
	static CURRENT: Reservation;
}

pub fn scope<T>(
	reservation: Reservation,
	future: impl Future<Output = T>,
) -> impl Future<Output = T> {
	CURRENT.scope(reservation, future)
}

fn lock_file(path: &Path) -> io::Result<File> {
	use std::os::unix::fs::OpenOptionsExt;
	let file = OpenOptions::new()
		.read(true)
		.write(true)
		.create(true)
		.truncate(false)
		.mode(0o600)
		.custom_flags(libc::O_NOFOLLOW)
		.open(path)?;
	// SAFETY: flock acts on a live descriptor and never dereferences pointers.
	if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
		return Err(io::Error::last_os_error());
	}
	Ok(file)
}

fn locked(path: &Path) -> anyhow::Result<bool> {
	match lock_file(path) {
		Ok(_) => Ok(false),
		Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(true),
		Err(error) => Err(error.into()),
	}
}

fn children_active(dir: &Path) -> anyhow::Result<bool> {
	for entry in fs::read_dir(dir)? {
		let entry = entry?;
		if entry.file_name().to_string_lossy().starts_with("child-") && locked(&entry.path())? {
			return Ok(true);
		}
	}
	Ok(false)
}

fn save(dir: &Path, record: &Record) -> anyhow::Result<()> {
	use io::Write;
	let mut file = File::create(dir.join("record.tmp"))?;
	file.write_all(&serde_json::to_vec(record)?)?;
	file.sync_all()?;
	fs::rename(dir.join("record.tmp"), dir.join("record.json"))?;
	File::open(dir)?.sync_all()?;
	Ok(())
}

impl Resources {
	pub fn open(config: ResourceConfig, data_dir: &Path) -> anyhow::Result<Self> {
		config.validate()?;
		let dir = data_dir.join("resources");
		if config.policy == ResourcePolicy::Legacy {
			ensure!(
				!dir.exists() || fs::read_dir(&dir)?.all(|e| e.is_ok_and(|e| !e.path().is_dir())),
				"resource reservations exist; restart with the previous resource policy and drain \
				 before disabling it"
			);
			return Ok(Self::default());
		}
		crate::vm::private_dir(&dir)?;
		let owner =
			lock_file(&dir.join("owner.lock")).context("another node owns this resource ledger")?;
		// One managed budget per host user also fences alternate data
		// directories. SAFETY: geteuid has no preconditions.
		let uid = unsafe { libc::geteuid() };
		let host_dir = Path::new("/tmp").join(format!("vmon-budget-{uid}"));
		match fs::create_dir(&host_dir) {
			Ok(()) => crate::vm::private_dir(&host_dir)?,
			Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {},
			Err(error) => return Err(error.into()),
		}
		use std::os::unix::fs::MetadataExt;
		let meta = fs::symlink_metadata(&host_dir)?;
		ensure!(
			meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0,
			"host budget lock directory must be private and owned by the node user"
		);
		let host_owner = lock_file(&host_dir.join("owner.lock"))
			.context("another node owns this user's host budget")?;
		let backend = platform::Backend::open(&config)?;
		let mut records = HashMap::new();
		for entry in fs::read_dir(&dir)? {
			let entry = entry?;
			if !entry.file_type()?.is_dir() {
				continue;
			}
			let id = entry
				.file_name()
				.into_string()
				.map_err(|_| anyhow::anyhow!("invalid reservation directory"))?;
			uuid::Uuid::parse_str(&id).context("invalid reservation ID")?;
			if !entry.path().join("record.json").exists() {
				ensure!(
					!locked(&entry.path().join("lease"))?
						&& !children_active(&entry.path())?
						&& !backend.active(&id)?,
					"live allocation has no durable resource record"
				);
				backend.remove(&id)?;
				fs::remove_dir_all(entry.path())?;
				continue;
			}
			let record = serde_json::from_slice(&fs::read(entry.path().join("record.json"))?)
				.context("reading durable resource reservation")?;
			records.insert(id, record);
		}
		let resources = Self(Some(Arc::new(Inner {
			config,
			dir,
			state: Mutex::new(State { records, pressure: false }),
			backend,
			_owner: owner,
			_host_owner: host_owner,
		})));
		let inner = resources.0.as_ref().expect("bounded resources");
		let mut state = inner.state.lock();
		inner.reap(&mut state)?;
		let (cpu, memory) = totals(&state);
		ensure!(
			cpu <= u64::from(inner.config.vcpus) && memory <= inner.config.memory_mb,
			"live reservations exceed the new budget; restore the previous budget and drain first"
		);
		inner.backend.recover(&state.records)?;
		drop(state);
		let sandboxes = data_dir.join("sandboxes");
		if sandboxes.is_dir() {
			for entry in fs::read_dir(sandboxes)? {
				let path = entry?.path().join("meta.json");
				if !path.is_file() {
					continue;
				}
				let meta: crate::sandbox::SandboxMeta = serde_json::from_slice(&fs::read(path)?)?;
				if let Some(pid) = meta.pid
					&& platform::identity(pid).is_ok()
				{
					resources
						.verify_adoption(&meta.sandbox_id, pid)
						.context("cannot enable node caps while unaccounted legacy VMMs are alive")?;
				}
			}
		}
		Ok(resources)
	}

	pub const fn bounded(&self) -> bool {
		self.0.is_some()
	}

	pub fn reserve(
		&self,
		name: &str,
		vcpus: u32,
		memory_mb: u64,
	) -> Result<Reservation, CapacityError> {
		self
			.reserve_inner(name, vcpus, memory_mb)
			.map_err(|error| CapacityError(format!("{error:#}")))
	}

	fn reserve_inner(&self, name: &str, vcpus: u32, memory_mb: u64) -> anyhow::Result<Reservation> {
		let Some(inner) = &self.0 else {
			return Ok(Reservation::default());
		};
		ensure!(vcpus > 0 && memory_mb > 0, "VM resources must be positive");
		let charged = memory_mb
			.checked_add(inner.config.overhead_mb)
			.context("memory charge overflow")?;
		let mut state = inner.state.lock();
		inner.reap(&mut state)?;
		inner.check(&mut state, u64::from(vcpus), charged)?;
		let id = uuid::Uuid::new_v4().to_string();
		let dir = inner.dir.join(&id);
		fs::create_dir(&dir)?;
		let record = Record {
			name: name.to_owned(),
			vcpus,
			memory_mb: charged,
			overhead_mb: inner.config.overhead_mb,
		};
		let result = (|| {
			let lock = lock_file(&dir.join("lease"))?;
			save(&dir, &record)?;
			inner.backend.create(&id, &record)?;
			anyhow::Ok(lock)
		})();
		let lock = match result {
			Ok(lock) => lock,
			Err(error) => {
				let _ = inner.backend.remove(&id);
				let _ = fs::remove_dir_all(&dir);
				return Err(error);
			},
		};
		state.records.insert(id.clone(), record);
		Ok(Reservation(Some(Arc::new(Lease { resources: self.clone(), id, _lock: lock }))))
	}

	pub fn for_launch(&self, name: &str, vcpus: u32, memory_mb: u64) -> anyhow::Result<Reservation> {
		let reservation = match CURRENT.try_with(Clone::clone) {
			Ok(reservation) if reservation.0.is_some() => reservation,
			_ => self.reserve(name, vcpus, memory_mb)?,
		};
		reservation
			.fit(vcpus, memory_mb)
			.map_err(|e| CapacityError(format!("{e:#}")))?;
		Ok(reservation)
	}

	pub fn capacity(&self) -> Option<ResourceCapacity> {
		let inner = self.0.as_ref()?;
		let mut state = inner.state.lock();
		let problem = inner
			.reap(&mut state)
			.and_then(|()| inner.check(&mut state, 0, 0))
			.err();
		let (cpu, memory) = totals(&state);
		let usage = inner.backend.usage().unwrap_or_default();
		Some(ResourceCapacity {
			version:            1,
			enforcement:        inner.backend.enforcement().into(),
			vcpus:              inner.config.vcpus,
			memory_mb:          inner.config.memory_mb,
			reserved_vcpus:     cpu,
			reserved_memory_mb: memory,
			overhead_mb:        inner.config.overhead_mb,
			pending:            state
				.records
				.keys()
				.filter(|id| !children_active(&inner.dir.join(id)).unwrap_or(true))
				.count() as u32,
			actual_memory_mb:   usage.actual_mb,
			host_available_mb:  usage.available_mb,
			memory_limit_mb:    usage.limit_mb,
			cpu_limit_millis:   usage.cpu_millis,
			admission_reason:   problem.map(|e| format!("{e:#}")),
		})
	}

	pub fn verify_adoption(&self, name: &str, pid: u32) -> anyhow::Result<()> {
		let Some(inner) = &self.0 else {
			return Ok(());
		};
		let state = inner.state.lock();
		for (id, record) in &state.records {
			if record.name == name
				&& children_active(&inner.dir.join(id))?
				&& inner.backend.contains(id, pid, &inner.dir.join(id))?
			{
				return Ok(());
			}
		}
		bail!(
			"VM {name} has no matching live resource reservation; drain legacy VMs before enabling \
			 caps"
		)
	}
}

fn totals(state: &State) -> (u64, u64) {
	state.records.values().fold((0u64, 0u64), |(cpu, mem), r| {
		(cpu.saturating_add(u64::from(r.vcpus)), mem.saturating_add(r.memory_mb))
	})
}

impl Inner {
	fn reap(&self, state: &mut State) -> anyhow::Result<()> {
		let mut dead = Vec::new();
		for id in state.records.keys() {
			let dir = self.dir.join(id);
			if !locked(&dir.join("lease"))? && !children_active(&dir)? && !self.backend.active(id)? {
				dead.push(id.clone());
			}
		}
		for id in dead {
			self.backend.remove(&id)?;
			fs::remove_dir_all(self.dir.join(&id))?;
			state.records.remove(&id);
		}
		Ok(())
	}

	fn check(&self, state: &mut State, cpu: u64, memory: u64) -> anyhow::Result<()> {
		let (reserved_cpu, reserved_memory) = totals(state);
		ensure!(
			cpu <= u64::from(self.config.vcpus).saturating_sub(reserved_cpu),
			"vCPU reservation budget exhausted"
		);
		ensure!(
			memory <= self.config.memory_mb.saturating_sub(reserved_memory),
			"memory reservation budget exhausted"
		);
		self.backend.verify()?;
		let usage = self.backend.usage()?;
		let unmaterialized = reserved_memory.saturating_sub(usage.materialized_mb);
		let remaining = usage
			.available_mb
			.saturating_sub(unmaterialized)
			.saturating_sub(memory);
		let floor = self
			.config
			.host_reserve_mb
			.saturating_add(if state.pressure {
				self.config.hysteresis_mb
			} else {
				0
			});
		state.pressure = usage.pressure || remaining < floor;
		ensure!(
			!state.pressure,
			"host memory pressure or reserve floor; retry after capacity recovers"
		);
		Ok(())
	}
}

impl Reservation {
	pub fn record_process(&self, pid: u32) -> anyhow::Result<()> {
		let Some(lease) = &self.0 else {
			return Ok(());
		};
		let inner = lease.resources.0.as_ref().expect("bounded reservation");
		let stamp = platform::identity(pid)?;
		let path = inner.dir.join(&lease.id).join("process.json");
		fs::write(&path, serde_json::to_vec(&(pid, stamp))?)?;
		File::open(path)?.sync_all()?;
		Ok(())
	}

	pub fn fit(&self, vcpus: u32, memory_mb: u64) -> anyhow::Result<()> {
		let Some(lease) = &self.0 else {
			return Ok(());
		};
		let inner = lease.resources.0.as_ref().expect("bounded reservation");
		let mut state = inner.state.lock();
		let previous = state
			.records
			.get(&lease.id)
			.context("missing reservation")?
			.clone();
		let memory_mb = memory_mb
			.checked_add(previous.overhead_mb)
			.context("memory charge overflow")?;
		let record = Record {
			vcpus: previous.vcpus.max(vcpus),
			memory_mb: previous.memory_mb.max(memory_mb),
			..previous.clone()
		};
		if record.vcpus != previous.vcpus || record.memory_mb != previous.memory_mb {
			inner.check(
				&mut state,
				u64::from(record.vcpus - previous.vcpus),
				record.memory_mb - previous.memory_mb,
			)?;
			save(&inner.dir.join(&lease.id), &record)?;
			state.records.insert(lease.id.clone(), record.clone());
			inner.backend.create(&lease.id, &record)?;
		}
		Ok(())
	}

	/// The inherited flock is the lifetime marker, independent of PID reuse.
	pub fn prepare(&self, command: &mut tokio::process::Command) -> anyhow::Result<Option<File>> {
		let Some(lease) = &self.0 else {
			return Ok(None);
		};
		let inner = lease.resources.0.as_ref().expect("bounded reservation");
		let _state = inner.state.lock();
		let dir = inner.dir.join(&lease.id);
		ensure!(
			!children_active(&dir)? && !inner.backend.active(&lease.id)?,
			"previous VM in this reservation is still stopping"
		);
		let file = lock_file(&dir.join(format!("child-{}", uuid::Uuid::new_v4())))?;
		let fd = file.as_raw_fd();
		let placement = inner.backend.placement(&lease.id)?;
		// SAFETY: the child only uses async-signal-safe fcntl/write syscalls;
		// the captured descriptors remain open through spawn.
		unsafe {
			command.pre_exec(move || {
				if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
					return Err(io::Error::last_os_error());
				}
				platform::place(&placement)
			});
		}
		Ok(Some(file))
	}
}

#[derive(Clone, Default)]
struct Usage {
	actual_mb:       Option<u64>,
	materialized_mb: u64,
	available_mb:    u64,
	limit_mb:        Option<u64>,
	cpu_millis:      Option<u64>,
	pressure:        bool,
}
