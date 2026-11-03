use std::{collections::HashMap, fs, io, path::Path};

use anyhow::{Context, ensure};
use mach2::{mach_init::mach_host_self, mach_port::mach_port_deallocate, traps::mach_task_self};

use super::{MIB, Record, ResourceConfig, ResourcePolicy, Usage};

pub(super) struct Backend {
	#[cfg(test)]
	pub(super) sample: Option<std::sync::Arc<parking_lot::Mutex<Usage>>>,
}

pub(super) struct Placement;

#[allow(
	clippy::unused_self,
	clippy::unnecessary_wraps,
	reason = "matches the fallible Linux enforcement interface"
)]
impl Backend {
	pub(super) fn open(config: &ResourceConfig) -> anyhow::Result<Self> {
		ensure!(
			config.policy == ResourcePolicy::Reserved,
			"macOS supports --resource-policy reserved; strict requires Linux cgroup v2. Reserved \
			 caps allocations, not aggregate host CPU/RAM usage"
		);
		Ok(Self {
			#[cfg(test)]
			sample:              None,
		})
	}

	pub(super) const fn enforcement(&self) -> &'static str {
		"admission"
	}

	pub(super) const fn create(&self, _: &str, _: &Record) -> anyhow::Result<()> {
		Ok(())
	}

	pub(super) const fn recover(&self, _: &HashMap<String, Record>) -> anyhow::Result<()> {
		Ok(())
	}

	pub(super) const fn active(&self, _: &str) -> anyhow::Result<bool> {
		Ok(false)
	}

	pub(super) const fn remove(&self, _: &str) -> anyhow::Result<()> {
		Ok(())
	}

	pub(super) const fn placement(&self, _: &str) -> anyhow::Result<Placement> {
		Ok(Placement)
	}

	pub(super) const fn verify(&self) -> anyhow::Result<()> {
		Ok(())
	}

	pub(super) fn contains(&self, _: &str, pid: u32, dir: &Path) -> anyhow::Result<bool> {
		let saved: (u32, u64) = serde_json::from_slice(&fs::read(dir.join("process.json"))?)?;
		Ok(saved.0 == pid && identity(pid).is_ok_and(|stamp| stamp == saved.1))
	}

	pub(super) fn usage(&self) -> anyhow::Result<Usage> {
		#[cfg(test)]
		if let Some(sample) = &self.sample {
			return Ok(sample.lock().clone());
		}
		// SAFETY: zero is a valid initial value for this C output struct.
		let mut stats: libc::vm_statistics64 = unsafe { std::mem::zeroed() };
		let mut count = libc::HOST_VM_INFO64_COUNT;
		// SAFETY: the pointer and count describe vm_statistics64 as required by
		// Mach.
		let result = unsafe {
			let host = mach_host_self();
			let result = libc::host_statistics64(
				host,
				libc::HOST_VM_INFO64,
				(&raw mut stats).cast(),
				&raw mut count,
			);
			mach_port_deallocate(mach_task_self(), host);
			result
		};
		ensure!(result == libc::KERN_SUCCESS, "host_statistics64 failed: {result}");
		// SAFETY: sysconf only reads the page size.
		let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) })
			.context("invalid page size")?;
		let mut pressure: libc::c_int = 0;
		let mut len = size_of::<libc::c_int>();
		// SAFETY: sysctl writes an integer into an appropriately sized buffer.
		let result = unsafe {
			libc::sysctlbyname(
				c"kern.memorystatus_vm_pressure_level".as_ptr(),
				(&raw mut pressure).cast(),
				&raw mut len,
				std::ptr::null_mut(),
				0,
			)
		};
		ensure!(result == 0, "reading macOS memory pressure: {}", io::Error::last_os_error());
		Ok(Usage {
			available_mb: (u64::from(stats.free_count)
				+ u64::from(stats.inactive_count.min(stats.external_page_count)))
				* page / MIB,
			pressure: pressure != 1,
			..Usage::default()
		})
	}
}

#[allow(clippy::unnecessary_wraps, reason = "matches the Linux pre-exec placement interface")]
pub(super) const fn place(_: &Placement) -> io::Result<()> {
	Ok(())
}

pub(super) fn identity(pid: u32) -> anyhow::Result<u64> {
	// SAFETY: zero is valid for the C output struct; proc_pid_rusage fills its
	// v2 layout.
	let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
	let pid = libc::pid_t::try_from(pid)?;
	// SAFETY: the buffer has the layout selected by RUSAGE_INFO_V2.
	let result = unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V2, (&raw mut info).cast()) };
	ensure!(result == 0, "reading process identity: {}", io::Error::last_os_error());
	Ok(info.ri_proc_start_abstime)
}
