use super::*;

fn fixture() -> (tempfile::TempDir, Resources, Arc<Mutex<Usage>>) {
	let dir = tempfile::tempdir().unwrap();
	let config = ResourceConfig {
		policy:          ResourcePolicy::Reserved,
		vcpus:           8,
		memory_mb:       256,
		overhead_mb:     16,
		host_reserve_mb: 128,
		hysteresis_mb:   32,
		cgroup:          None,
	};
	let sample = Arc::new(Mutex::new(Usage { available_mb: 4096, ..Usage::default() }));
	let mut backend = platform::Backend::open(&config).unwrap();
	backend.sample = Some(sample.clone());
	let resources = Resources(Some(Arc::new(Inner {
		config,
		dir: dir.path().to_owned(),
		state: Mutex::default(),
		backend,
		_owner: lock_file(&dir.path().join("owner.lock")).unwrap(),
		_host_owner: lock_file(&dir.path().join("host.lock")).unwrap(),
	})));
	(dir, resources, sample)
}

#[test]
fn racing_requests_share_one_budget_and_charge_overhead() {
	let (_dir, resources, _) = fixture();
	let barrier = std::sync::Barrier::new(32);
	let held = std::thread::scope(|scope| {
		let handles: Vec<_> = (0..32)
			.map(|i| {
				let resources = &resources;
				let barrier = &barrier;
				scope.spawn(move || {
					barrier.wait();
					resources.reserve(&format!("vm-{i}"), 2, 48).ok()
				})
			})
			.collect();
		handles
			.into_iter()
			.filter_map(|h| h.join().unwrap())
			.collect::<Vec<_>>()
	});
	assert_eq!(held.len(), 4);
	let cap = resources.capacity().unwrap();
	assert_eq!((cap.reserved_vcpus, cap.reserved_memory_mb, cap.pending), (8, 256, 4));
	drop(held);
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 0);
}

#[tokio::test]
async fn child_holds_budget_after_request_is_cancelled_and_after_recovery() {
	let (dir, resources, _) = fixture();
	let reservation = resources.reserve("sandbox", 2, 48).unwrap();
	let mut command = tokio::process::Command::new("sleep");
	command.arg("30").kill_on_drop(true);
	let child_lease = reservation.prepare(&mut command).unwrap();
	let mut child = command.spawn().unwrap();
	reservation.record_process(child.id().unwrap()).unwrap();
	drop(child_lease);
	drop(reservation);
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 64);
	resources
		.verify_adoption("sandbox", child.id().unwrap())
		.unwrap();
	assert!(
		resources
			.verify_adoption("sandbox", std::process::id())
			.is_err()
	);
	let saved = resources.0.as_ref().unwrap().state.lock().records.clone();
	drop(resources);
	let (_other_dir, recovered, _) = fixture();
	let inner = recovered.0.as_ref().unwrap();
	for (id, record) in saved {
		let target = inner.dir.join(&id);
		// Recovery reads the same on-disk locks, including the child's open
		// inode.
		fs::rename(dir.path().join(&id), &target).unwrap();
		inner.state.lock().records.insert(id, record);
	}
	assert_eq!(recovered.capacity().unwrap().reserved_memory_mb, 64);
	child.kill().await.unwrap();
	child.wait().await.unwrap();
	assert_eq!(recovered.capacity().unwrap().reserved_memory_mb, 0);
}

#[tokio::test]
async fn failed_spawn_and_abandoned_operation_release_reservations() {
	let (_dir, resources, _) = fixture();
	let reservation = resources.reserve("build", 2, 48).unwrap();
	let mut command = tokio::process::Command::new("/nonexistent-vmon-resource-test");
	let lease = reservation.prepare(&mut command).unwrap();
	assert!(command.spawn().is_err());
	drop(command);
	drop(lease);
	drop(reservation);
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 0);
}

#[tokio::test]
async fn scoped_build_phases_reuse_peak_reservation_and_snapshot_size_is_charged() {
	let (_dir, resources, _) = fixture();
	let job = resources.reserve("build", 2, 48).unwrap();
	scope(job, async {
		let phase = resources.for_launch("build-unpack", 2, 32).unwrap();
		assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 64);
		phase.fit(4, 128).unwrap();
		assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 144);
		assert!(phase.fit(4, 256).is_err());
		assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 144);
	})
	.await;
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 0);
}

#[test]
fn host_pressure_has_hysteresis_and_charges_unmaterialized_commitments() {
	let (_dir, resources, sample) = fixture();
	let held = resources.reserve("vm", 2, 48).unwrap();
	sample.lock().available_mb = 200;
	assert!(resources.reserve("second", 2, 48).is_err());
	sample.lock().available_mb = 270;
	assert!(resources.reserve("second", 2, 48).is_err());
	sample.lock().available_mb = 288;
	let second = resources.reserve("second", 2, 48).unwrap();
	sample.lock().pressure = true;
	assert!(resources.capacity().unwrap().admission_reason.is_some());
	drop((held, second));
}

#[test]
fn invalid_or_overflowing_budgets_fail_before_work_starts() {
	let (_dir, resources, _) = fixture();
	assert!(resources.reserve("invalid", 0, 10).is_err());
	assert!(resources.reserve("overflow", 1, u64::MAX).is_err());
	assert!(resources.reserve("cpu", 9, 10).is_err());
	let mut config = resources.0.as_ref().unwrap().config.clone();
	config.memory_mb = u64::MAX;
	assert!(config.validate().is_err());
}

#[test]
fn strict_policy_never_falls_back_without_delegation() {
	let dir = tempfile::tempdir().unwrap();
	let config = ResourceConfig {
		policy:          ResourcePolicy::Strict,
		vcpus:           1,
		memory_mb:       64,
		overhead_mb:     16,
		host_reserve_mb: 0,
		hysteresis_mb:   0,
		cgroup:          Some(dir.path().to_owned()),
	};
	assert!(platform::Backend::open(&config).is_err());
}

#[tokio::test]
async fn crash_writer() {
	let Ok(path) = std::env::var("VMON_RESOURCE_CRASH_DIR") else {
		return;
	};
	let resources = Resources::open(crash_config(), Path::new(&path)).unwrap();
	let reservation = resources.reserve("survivor", 1, 48).unwrap();
	let mut command = tokio::process::Command::new("sleep");
	command.arg("20");
	let inherited = reservation.prepare(&mut command).unwrap();
	let child = command.spawn().unwrap();
	reservation.record_process(child.id().unwrap()).unwrap();
	fs::write(Path::new(&path).join("pid"), child.id().unwrap().to_string()).unwrap();
	drop(inherited);
	// SAFETY: this fixture terminates only its own isolated test process.
	unsafe {
		libc::kill(libc::getpid(), libc::SIGKILL);
	}
}

fn crash_config() -> ResourceConfig {
	ResourceConfig {
		policy:          ResourcePolicy::Reserved,
		vcpus:           1,
		memory_mb:       64,
		overhead_mb:     16,
		host_reserve_mb: 0,
		hysteresis_mb:   0,
		cgroup:          None,
	}
}

#[tokio::test]
async fn sigkill_restart_recovers_live_reservations_and_fences_duplicate_daemons() {
	let dir = tempfile::tempdir().unwrap();
	let status = tokio::process::Command::new(std::env::current_exe().unwrap())
		.args(["--exact", "resources::tests::crash_writer", "--nocapture"])
		.env("VMON_RESOURCE_CRASH_DIR", dir.path())
		.status()
		.await
		.unwrap();
	use std::os::unix::process::ExitStatusExt;
	assert_eq!(status.signal(), Some(libc::SIGKILL));
	let pid: u32 = fs::read_to_string(dir.path().join("pid"))
		.unwrap()
		.parse()
		.unwrap();
	let resources = Resources::open(crash_config(), dir.path()).unwrap();
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 64);
	assert!(resources.reserve("second", 1, 1).is_err());
	let other = tempfile::tempdir().unwrap();
	assert!(Resources::open(crash_config(), other.path()).is_err());
	resources.verify_adoption("survivor", pid).unwrap();
	// SAFETY: the live reservation and process start time identify this
	// fixture's child.
	unsafe {
		libc::kill(libc::pid_t::try_from(pid).unwrap(), libc::SIGKILL);
	}
	for _ in 0..100 {
		if resources.capacity().unwrap().reserved_memory_mb == 0 {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	}
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn delegated_cgroup_enforces_cpu_and_cow_memory() {
	if std::env::var("VMON_CGROUP_TEST").as_deref() != Ok("1") {
		return;
	}
	let dir = tempfile::tempdir().unwrap();
	let config = ResourceConfig {
		policy:          ResourcePolicy::Strict,
		vcpus:           1,
		memory_mb:       64,
		overhead_mb:     16,
		host_reserve_mb: 0,
		hysteresis_mb:   0,
		cgroup:          None,
	};
	let resources = Resources::open(config, dir.path()).unwrap();
	let reservation = resources.reserve("cpu", 1, 48).unwrap();
	let lease = reservation.0.as_ref().unwrap();
	let root = fs::read_to_string("/proc/self/cgroup").unwrap();
	let root = root.lines().find_map(|s| s.strip_prefix("0::")).unwrap();
	let root = Path::new("/sys/fs/cgroup")
		.join(root.trim_start_matches('/'))
		.parent()
		.unwrap()
		.to_owned();
	let group = root.join("workloads").join(&lease.id);
	let mut cpu = tokio::process::Command::new("python3");
	cpu.arg("-c")
		.arg(
			"import os,time\nend=time.monotonic()+1.5\nfor i in range(2):\n if os.fork()==0:\n  \
			 while time.monotonic()<end: pass\n  os._exit(0)\nwhile time.monotonic()<end: pass\nfor \
			 i in range(2): os.wait()\n",
		)
		.kill_on_drop(true);
	let inherited = reservation.prepare(&mut cpu).unwrap();
	let mut child = cpu.spawn().unwrap();
	drop(inherited);
	let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
		.await
		.unwrap()
		.unwrap();
	assert!(status.success());
	let stat = fs::read_to_string(group.join("cpu.stat")).unwrap();
	let throttled: u64 = stat
		.lines()
		.find_map(|s| s.strip_prefix("nr_throttled "))
		.unwrap()
		.parse()
		.unwrap();
	assert!(throttled > 0, "{stat}");
	let mut memory = tokio::process::Command::new("python3");
	memory
		.arg("-c")
		.arg(
			"import os,time\na=bytearray(24*1024*1024)\nfor i in range(3):\n if os.fork()==0:\n  for \
			 j in range(0,len(a),4096): a[j]=1\n  time.sleep(5)\n  os._exit(0)\ntime.sleep(5)\n",
		)
		.kill_on_drop(true);
	let inherited = reservation.prepare(&mut memory).unwrap();
	let mut child = memory.spawn().unwrap();
	drop(inherited);
	let status = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
		.await
		.unwrap()
		.unwrap();
	assert!(!status.success(), "CoW expansion must hit the tiny workload ceiling");
	let events = fs::read_to_string(group.join("memory.events")).unwrap();
	let kills: u64 = events
		.lines()
		.find_map(|s| s.strip_prefix("oom_kill "))
		.unwrap()
		.parse()
		.unwrap();
	assert!(kills > 0, "{events}");
	drop(reservation);
	for _ in 0..100 {
		if resources.capacity().unwrap().reserved_memory_mb == 0 {
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(20)).await;
	}
	assert_eq!(resources.capacity().unwrap().reserved_memory_mb, 0);
	assert_eq!(resources.capacity().unwrap().enforcement, "cgroup-v2");
}
