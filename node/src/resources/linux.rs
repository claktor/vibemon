use std::{
	collections::HashMap,
	fs::{self, File, OpenOptions},
	io,
	os::fd::AsRawFd,
	path::{Path, PathBuf},
};

use anyhow::{Context, ensure};

use super::{MIB, Record, ResourceConfig, ResourcePolicy, Usage};

pub(super) struct Backend {
	#[cfg(test)]
	pub(super) sample: Option<std::sync::Arc<parking_lot::Mutex<Usage>>>,
	root:              Option<PathBuf>,
	memory_max:        u64,
	cpu_max:           u64,
	pool_memory:       u64,
	pool_cpu:          u64,
	events:            parking_lot::Mutex<(u64, Option<std::time::Instant>)>,
}

fn read(path: &Path, file: &str) -> anyhow::Result<String> {
	fs::read_to_string(path.join(file))
		.with_context(|| format!("reading {}", path.join(file).display()))
}

fn number(path: &Path, file: &str) -> anyhow::Result<u64> {
	Ok(read(path, file)?.trim().parse()?)
}

fn value(text: &str, key: &str) -> anyhow::Result<u64> {
	text
		.lines()
		.find_map(|line| {
			let mut parts = line.split_whitespace();
			(parts.next()? == key)
				.then(|| parts.next()?.parse().ok())
				.flatten()
		})
		.with_context(|| format!("missing {key}"))
}

fn current_group(pid: u32) -> anyhow::Result<PathBuf> {
	let text = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
	let group = text
		.lines()
		.find_map(|line| line.strip_prefix("0::"))
		.context("cgroup v2 is required")?;
	ensure!(
		!Path::new(group)
			.components()
			.any(|p| p == std::path::Component::ParentDir),
		"cgroup namespace must expose the delegated hierarchy"
	);
	Ok(Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/')))
}

fn ceilings(root: &Path) -> anyhow::Result<(u64, u64, u64, u64)> {
	let (mut memory, mut cpu, mut high, mut swap) = (u64::MAX, u64::MAX, u64::MAX, u64::MAX);
	for dir in root
		.ancestors()
		.take_while(|p| p.starts_with("/sys/fs/cgroup"))
	{
		if !dir.join("memory.max").exists() {
			continue;
		}
		ensure!(
			number(dir, "memory.oom.group")? == 0,
			"whole-service OOM grouping must be disabled for strict nodes"
		);
		for (name, limit) in
			[("memory.max", &mut memory), ("memory.high", &mut high), ("memory.swap.max", &mut swap)]
		{
			let text = read(dir, name)?;
			if text.trim() != "max" {
				*limit = (*limit).min(text.trim().parse()?);
			}
		}
		let raw = read(dir, "cpu.max")?;
		let mut parts = raw.split_whitespace();
		let quota = parts.next().context("missing cpu quota")?;
		if quota != "max" {
			let quota: u64 = quota.parse()?;
			let period: u64 = parts.next().context("missing cpu period")?.parse()?;
			ensure!(period > 0, "invalid cpu period");
			cpu = cpu.min(quota.checked_mul(1000).context("CPU quota overflow")? / period);
		}
	}
	ensure!(cpu != u64::MAX, "strict policy requires finite parent cpu.max");
	let status = fs::read_to_string("/proc/self/status")?;
	let allowed = status
		.lines()
		.find_map(|line| line.strip_prefix("Cpus_allowed_list:"))
		.context("missing effective CPU affinity")?;
	cpu = cpu.min(
		cpu_count(allowed)?
			.checked_mul(1000)
			.context("CPU affinity overflow")?,
	);
	Ok((memory, cpu, high, swap))
}

fn cpu_count(list: &str) -> anyhow::Result<u64> {
	let mut count = 0u64;
	for part in list.trim().split(',') {
		let (first, last) = part.split_once('-').unwrap_or((part, part));
		let (first, last) = (first.parse::<u64>()?, last.parse::<u64>()?);
		let range = last
			.checked_sub(first)
			.and_then(|n| n.checked_add(1))
			.context("invalid CPU affinity range")?;
		count = count.checked_add(range).context("CPU affinity overflow")?;
	}
	ensure!(count > 0, "empty CPU affinity");
	Ok(count)
}

#[cfg(test)]
#[test]
fn effective_cpu_ranges_are_counted_without_overcommit() {
	assert_eq!(cpu_count("0-3,8,10-11\n").unwrap(), 7);
	assert!(cpu_count("4-1").is_err());
	assert!(cpu_count("").is_err());
}

fn write(dir: &Path, name: &str, value: impl std::fmt::Display) -> anyhow::Result<()> {
	fs::write(dir.join(name), value.to_string())
		.with_context(|| format!("writing {} (requires cgroup delegation)", dir.join(name).display()))
}

impl Backend {
	pub(super) fn open(config: &ResourceConfig) -> anyhow::Result<Self> {
		if config.policy == ResourcePolicy::Reserved {
			return Ok(Self {
				root:                None,
				memory_max:          0,
				cpu_max:             0,
				pool_memory:         0,
				pool_cpu:            0,
				events:              Default::default(),
				#[cfg(test)]
				sample:              None,
			});
		}
		let current = current_group(std::process::id())?.canonicalize()?;
		let root = config
			.cgroup
			.clone()
			.unwrap_or_else(|| {
				if current.file_name().is_some_and(|n| n == "manager") {
					current.parent().expect("cgroup parent").to_owned()
				} else {
					current.clone()
				}
			})
			.canonicalize()?;
		ensure!(
			root.starts_with("/sys/fs/cgroup") && (current == root || current == root.join("manager")),
			"node must run inside its delegated cgroup or manager subgroup"
		);
		let (memory_max, cpu_max, high, swap) = ceilings(&root)?;
		ensure!(
			memory_max != u64::MAX && cpu_max != u64::MAX,
			"strict policy requires finite parent memory.max and cpu.max"
		);
		ensure!(
			high < memory_max && swap == 0,
			"strict policy requires memory.high below memory.max and memory.swap.max=0"
		);
		let pool_memory = config.memory_mb * MIB;
		let pool_cpu = u64::from(config.vcpus) * 1000;
		ensure!(
			pool_memory < high && pool_cpu < cpu_max,
			"workload budget must leave memory and CPU headroom for the manager"
		);
		ensure!(
			number(&root, "memory.current")? < high,
			"cgroup is already above its pressure threshold"
		);
		let controllers = read(&root, "cgroup.controllers")?;
		for controller in ["cpu", "memory", "pids"] {
			ensure!(
				controllers.split_whitespace().any(|c| c == controller),
				"missing delegated {controller} controller"
			);
		}
		let manager = root.join("manager");
		fs::create_dir_all(&manager)?;
		if current == root {
			let processes = read(&root, "cgroup.procs")?;
			ensure!(
				processes
					.lines()
					.all(|p| p.parse::<u32>().ok() == Some(std::process::id())),
				"delegated root contains other processes; use DelegateSubgroup=manager"
			);
			write(&manager, "cgroup.procs", std::process::id())?;
		}
		write(&root, "cgroup.subtree_control", "+cpu +memory +pids")?;
		let workloads = root.join("workloads");
		fs::create_dir_all(&workloads)?;
		if workloads.join("memory.current").exists() {
			if value(&read(&workloads, "cgroup.events")?, "populated")? != 0 {
				let old_cpu = read(&workloads, "cpu.max")?;
				let expected_cpu = format!("{} 100000", pool_cpu * 100);
				ensure!(
					number(&workloads, "memory.max")? <= pool_memory && old_cpu.trim() == expected_cpu,
					"cannot shrink memory or change CPU caps with live workloads; drain first"
				);
			}
			ensure!(
				number(&workloads, "memory.current")? < pool_memory,
				"workloads already exceed the proposed memory budget; drain first"
			);
		}
		write(&workloads, "memory.max", pool_memory)?;
		write(&workloads, "memory.swap.max", 0)?;
		write(&workloads, "cpu.max", format!("{} 100000", pool_cpu * 100))?;
		write(&workloads, "pids.max", 4096)?;
		write(&workloads, "cgroup.subtree_control", "+cpu +memory +pids")?;
		Ok(Self {
			root: Some(root),
			memory_max,
			cpu_max,
			pool_memory,
			pool_cpu,
			events: Default::default(),
			#[cfg(test)]
			sample: None,
		})
	}

	fn group(&self, id: &str) -> Option<PathBuf> {
		self
			.root
			.as_ref()
			.map(|root| root.join("workloads").join(id))
	}

	pub(super) const fn enforcement(&self) -> &'static str {
		if self.root.is_some() {
			"cgroup-v2"
		} else {
			"admission"
		}
	}

	pub(super) fn create(&self, id: &str, record: &Record) -> anyhow::Result<()> {
		if let Some(group) = self.group(id) {
			fs::create_dir_all(&group)?;
			write(
				&group,
				"memory.max",
				record
					.memory_mb
					.checked_mul(MIB)
					.context("memory limit overflow")?,
			)?;
			write(&group, "memory.swap.max", 0)?;
			write(&group, "memory.oom.group", 1)?;
			write(&group, "cpu.max", format!("{} 100000", u64::from(record.vcpus) * 100000))?;
			write(&group, "pids.max", 512)?;
		}
		Ok(())
	}

	pub(super) fn recover(&self, records: &HashMap<String, Record>) -> anyhow::Result<()> {
		if let Some(root) = &self.root {
			for entry in fs::read_dir(root.join("workloads"))? {
				let entry = entry?;
				if entry.file_type()?.is_dir() {
					let id = entry.file_name().to_string_lossy().into_owned();
					ensure!(
						records.contains_key(&id) || !self.active(&id)?,
						"unaccounted live workload cgroup {id}; restore its ledger before admitting work"
					);
					if !records.contains_key(&id) {
						self.remove(&id)?;
					}
				}
			}
			for (id, record) in records {
				self.create(id, record)?;
			}
		}
		Ok(())
	}

	pub(super) fn active(&self, id: &str) -> anyhow::Result<bool> {
		let Some(group) = self.group(id).filter(|g| g.exists()) else {
			return Ok(false);
		};
		Ok(value(&read(&group, "cgroup.events")?, "populated")? != 0)
	}

	pub(super) fn remove(&self, id: &str) -> anyhow::Result<()> {
		if let Some(group) = self.group(id).filter(|g| g.exists()) {
			fs::remove_dir(group)?;
		}
		Ok(())
	}

	pub(super) fn placement(&self, id: &str) -> anyhow::Result<Option<File>> {
		self
			.group(id)
			.map(|group| {
				OpenOptions::new()
					.write(true)
					.open(group.join("cgroup.procs"))
			})
			.transpose()
			.map_err(Into::into)
	}

	pub(super) fn contains(&self, id: &str, pid: u32, dir: &Path) -> anyhow::Result<bool> {
		match self.group(id) {
			Some(group) => Ok(current_group(pid)? == group && identity_matches(pid, dir)?),
			None => identity_matches(pid, dir),
		}
	}

	pub(super) fn verify(&self) -> anyhow::Result<()> {
		if let Some(root) = &self.root {
			let (memory, cpu, high, swap) = ceilings(root)?;
			ensure!(
				memory <= self.memory_max && cpu <= self.cpu_max && high < memory && swap == 0,
				"parent cgroup limits changed or enforcement was disabled"
			);
			ensure!(
				self.pool_memory < high && self.pool_cpu < cpu,
				"parent cgroup budget shrank; drain and restart with a smaller workload budget"
			);
			let workloads = root.join("workloads");
			ensure!(
				number(&workloads, "memory.max")? == self.pool_memory
					&& number(&workloads, "memory.swap.max")? == 0,
				"workload memory enforcement changed"
			);
			ensure!(
				read(&workloads, "cpu.max")?.trim() == format!("{} 100000", self.pool_cpu * 100),
				"workload CPU enforcement changed"
			);
			ensure!(
				current_group(std::process::id())? == root.join("manager"),
				"node escaped its delegated manager cgroup"
			);
		}
		Ok(())
	}

	pub(super) fn usage(&self) -> anyhow::Result<Usage> {
		#[cfg(test)]
		if let Some(sample) = &self.sample {
			return Ok(sample.lock().clone());
		}
		let available_mb = value(&fs::read_to_string("/proc/meminfo")?, "MemAvailable:")? / 1024;
		let mut usage = Usage { available_mb, ..Usage::default() };
		let psi = fs::read_to_string("/proc/pressure/memory")?;
		usage.pressure = pressure(&psi)?;
		if let Some(root) = &self.root {
			let (memory, cpu, high, _) = ceilings(root)?;
			let actual = number(root, "memory.current")?;
			usage.actual_mb = Some(actual / MIB);
			usage.limit_mb = Some(memory / MIB);
			usage.cpu_millis = Some(cpu);
			usage.materialized_mb =
				value(&read(&root.join("workloads"), "memory.stat")?, "anon")? / MIB;
			usage.pressure |= actual >= high || pressure(&read(root, "memory.pressure")?)?;
			let events = read(root, "memory.events")?;
			let count = value(&events, "high")?
				.saturating_add(value(&events, "max")?)
				.saturating_add(value(&events, "oom")?);
			let mut observed = self.events.lock();
			if observed.0 != count {
				*observed = (count, Some(std::time::Instant::now()));
			}
			usage.pressure |= observed
				.1
				.is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(5));
		}
		Ok(usage)
	}
}

fn pressure(text: &str) -> anyhow::Result<bool> {
	let line = text
		.lines()
		.find(|line| line.starts_with("some "))
		.context("missing memory PSI")?;
	let average: f64 = line
		.split_whitespace()
		.find_map(|s| s.strip_prefix("avg10="))
		.context("missing PSI avg10")?
		.parse()?;
	Ok(average >= 1.0)
}

#[allow(
	clippy::ref_option,
	reason = "opaque placement token borrowed through the common platform interface"
)]
pub(super) fn place(placement: &Option<File>) -> io::Result<()> {
	if let Some(file) = placement {
		// SAFETY: the inherited descriptor is open; "0" moves the calling child.
		if unsafe { libc::write(file.as_raw_fd(), b"0".as_ptr().cast(), 1) } != 1 {
			return Err(io::Error::last_os_error());
		}
	}
	Ok(())
}

pub(super) fn identity(pid: u32) -> anyhow::Result<u64> {
	let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
	Ok(stat
		.rsplit_once(") ")
		.context("invalid process stat")?
		.1
		.split_whitespace()
		.nth(19)
		.context("missing process start time")?
		.parse()?)
}

pub(super) fn identity_matches(pid: u32, dir: &Path) -> anyhow::Result<bool> {
	let saved: (u32, u64) = serde_json::from_slice(&fs::read(dir.join("process.json"))?)?;
	Ok(saved.0 == pid && identity(pid).is_ok_and(|stamp| stamp == saved.1))
}
