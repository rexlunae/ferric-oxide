//! The QEMU machine a run boots.
//!
//! [`LaunchConfig`] holds every option of a run; [`plan`] turns it, with the facts about the
//! host ([`HostFacts`]: KVM, firmware), into the QEMU command line. Planning is pure so the
//! command line can be tested without QEMU.
//!
//! The topology is the one `scripts/ferric-ci/boot-smoke.sh` uses, so the guest sees the
//! same PCI addresses and its checks (`check-iommu-log.sh` finds drivers by requester ID)
//! keep working: q35, the NVMe disk at `00:04.0`, the NIC (e1000 or virtio-net) at
//! `00:05.0`, QEMU's `edu` DMA test device at `00:10.0` with a full DMA mask, and two HDA
//! controllers. The user network serves a private TFTP root holding the netq test's fixture.
//! With a display (`--display std`, the default) the machine also gets `-vga std` and a
//! `qemu-xhci` controller, as `make qemu` does; `--input usb` adds a USB tablet and keyboard.
//!
//! Disks are always opened with `snapshot=on`: guest writes go to a temporary overlay in the
//! run directory and the image is never modified. The guest starts paused (`-S`); the
//! harness connects to the QMP and serial sockets first and then resumes it, so no console
//! output is lost. In trace mode the guest RAM is a shared file under `/dev/shm`, which
//! `ferric-trace follow` maps read-only.

use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};

use clap::ValueEnum;
use ferric_abi::boot_env::Kernel;
use serde::{Deserialize, Serialize};

use crate::bootenv::{BootEnvVia, FW_CFG_FILE};
use crate::error::Error;
use crate::rundir::RunDir;

/// Normal or trace kernel.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// The image's normal kernel, which must print no `trace:` lines.
    #[default]
    Normal,
    /// The trace-mode kernel (`usr/lib/boot/kernel-trace`) with host-visible rings.
    Trace,
}

impl Mode {
    /// The kernel this mode boots; its name is the `FERRIC_KERNEL` value that selects it.
    pub const fn kernel(self) -> Kernel {
        match self {
            Mode::Normal => Kernel::Normal,
            Mode::Trace => Kernel::Trace,
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kernel().name())
    }
}

/// The guest's display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Display {
    /// A standard VGA framebuffer (no window; the screen is read through QMP).
    #[default]
    Std,
    /// No display device, as CI's smoke boots run; the bootloader shows no menu.
    None,
}

/// Which accelerator QEMU uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Accel {
    /// KVM when `/dev/kvm` can be opened, TCG otherwise.
    #[default]
    Auto,
    /// KVM, or fail.
    Kvm,
    /// TCG software emulation.
    Tcg,
}

/// Extra input devices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum InputDevices {
    /// The q35 PS/2 keyboard and mouse, with QEMU's vmmouse as the absolute pointer.
    #[default]
    Ps2,
    /// Also a USB tablet and keyboard on the xHCI controller.
    Usb,
}

/// The network card.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Nic {
    /// Intel e1000 (e1000d).
    #[default]
    E1000,
    /// virtio-net (virtio-netd).
    Virtio,
}

/// An emulated IOMMU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Iommu {
    /// None.
    #[default]
    None,
    /// Intel VT-d with interrupt remapping (split irqchip).
    Vtd,
    /// AMD-Vi with DMA remapping.
    Amdvi,
}

/// What to do when a crash line appears on the console.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum OnCrash {
    /// Stop the run two seconds later (so the backtrace reaches the log), as boot-smoke does.
    #[default]
    Stop,
    /// Record it and keep running.
    Continue,
}

/// Every option of a run. The supervisor reads it from `<run>/launch.json`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LaunchConfig {
    /// The disk image (absolute). It is opened with `snapshot=on`.
    pub image: PathBuf,
    /// The run directory (absolute).
    pub run_dir: PathBuf,
    /// The repository root, when the harness runs inside a checkout.
    pub repo_root: Option<PathBuf>,
    /// Normal or trace kernel.
    pub mode: Mode,
    /// Display or headless.
    pub display: Display,
    /// `KEY=VALUE` boot environment entries from the command line.
    pub boot_env: Vec<(String, String)>,
    /// How the boot environment reaches the bootloader.
    pub boot_env_via: BootEnvVia,
    /// Host-visible trace ring size per CPU, in KiB (trace mode).
    pub trace_ring_kib: u64,
    /// The `ferric-trace` binary (resolved when trace mode is requested).
    pub ferric_trace: Option<PathBuf>,
    /// Start `ferric-trace follow` as soon as the kernel announces its rings.
    pub trace_autostart: bool,
    /// vCPUs.
    pub smp: u32,
    /// Guest RAM in MiB.
    pub mem_mib: u64,
    /// Accelerator.
    pub accel: Accel,
    /// CPU model; defaults to `host,migratable=off` under KVM and `max` under TCG.
    pub cpu: Option<String>,
    /// Extra input devices.
    pub input: InputDevices,
    /// Network card.
    pub nic: Nic,
    /// Emulated IOMMU.
    pub iommu: Iommu,
    /// More disks, each attached as a snapshot NVMe namespace.
    pub extra_disks: Vec<PathBuf>,
    /// Let the guest reset (no `-no-reboot`).
    pub reboot: bool,
    /// Stop the run after this many seconds.
    pub timeout_s: Option<f64>,
    /// What a crash line does.
    pub on_crash: OnCrash,
    /// Seconds between stats samples (0 samples only at the end).
    pub stats_interval_s: f64,
    /// UEFI firmware; searched for when absent.
    pub firmware: Option<PathBuf>,
    /// The QEMU program.
    pub qemu: PathBuf,
    /// Arguments placed between the program and QEMU's own (for a wrapper such as
    /// `taskset -c 0-3 qemu-system-x86_64`).
    pub qemu_wrapper_args: Vec<String>,
    /// Arguments appended to the generated command line.
    pub qemu_args: Vec<String>,
    /// Require the smoke protocol (`FERRIC_SMOKE_OK` and a test result).
    pub smoke: bool,
    /// Milliseconds between keyboard events, so the PS/2 queue never overflows.
    pub key_delay_ms: u64,
    /// Seconds `down` waits for the guest to act on the ACPI power button before quitting.
    pub powerdown_timeout_s: f64,
}

impl LaunchConfig {
    /// A configuration with the defaults for `image` in `run_dir`.
    pub fn new(image: PathBuf, run_dir: PathBuf) -> Self {
        Self {
            image,
            run_dir,
            repo_root: None,
            mode: Mode::Normal,
            display: Display::Std,
            boot_env: Vec::new(),
            boot_env_via: BootEnvVia::FwCfg,
            trace_ring_kib: 4096,
            ferric_trace: None,
            trace_autostart: true,
            smp: 4,
            mem_mib: 2048,
            accel: Accel::Auto,
            cpu: None,
            input: InputDevices::Ps2,
            nic: Nic::E1000,
            iommu: Iommu::None,
            extra_disks: Vec::new(),
            reboot: false,
            timeout_s: None,
            on_crash: OnCrash::Stop,
            stats_interval_s: 5.0,
            firmware: None,
            qemu: PathBuf::from("qemu-system-x86_64"),
            qemu_wrapper_args: Vec::new(),
            qemu_args: Vec::new(),
            smoke: false,
            key_delay_ms: 15,
            powerdown_timeout_s: 20.0,
        }
    }

    /// The shared guest-RAM file of a trace-mode run.
    pub fn shm_path(&self) -> Option<PathBuf> {
        (self.mode == Mode::Trace).then(|| {
            let name = RunDir::new(&self.run_dir).name();
            PathBuf::from(format!("/dev/shm/ferric-harness-{name}"))
        })
    }
}

/// UEFI firmware and how QEMU loads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    /// A combined image loaded with `-bios`.
    Bios(PathBuf),
    /// A code-only image mapped read-only as the first pflash unit.
    Pflash(PathBuf),
}

/// The `-bios` images `boot-smoke.sh` and `mk/qemu.mk` look for, in order.
pub const BIOS_CANDIDATES: [&str; 3] = [
    "/usr/share/ovmf/OVMF.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/qemu/OVMF.fd",
];
/// The pflash images `mk/qemu.mk` falls back to.
pub const PFLASH_CANDIDATES: [&str; 5] = [
    "/usr/share/qemu/edk2-x86_64-code.fd",
    "/run/libvirt/nix-ovmf/edk2-x86_64-code.fd",
    "/opt/homebrew/opt/qemu/share/qemu/edk2-x86_64-code.fd",
    "/usr/local/share/qemu/edk2-x86_64-code.fd",
    "/opt/local/share/qemu/edk2-x86_64-code.fd",
];

/// Finds the firmware: `explicit` (or `OVMF_FIRMWARE`, which the caller passes in) first,
/// then the usual install paths.
///
/// # Errors
///
/// [`Error::NoFirmware`] when nothing is found.
pub fn find_firmware(explicit: Option<&Path>) -> Result<Firmware, Error> {
    if let Some(path) = explicit {
        return if path.is_file() {
            Ok(Firmware::Bios(path.to_path_buf()))
        } else {
            Err(Error::NoFirmware {
                searched: vec![path.to_path_buf()],
            })
        };
    }
    BIOS_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .map(Firmware::Bios)
        .or_else(|| {
            PFLASH_CANDIDATES
                .iter()
                .map(PathBuf::from)
                .find(|p| p.is_file())
                .map(Firmware::Pflash)
        })
        .ok_or_else(|| Error::NoFirmware {
            searched: BIOS_CANDIDATES
                .iter()
                .chain(&PFLASH_CANDIDATES)
                .map(PathBuf::from)
                .collect(),
        })
}

/// Whether this process can use KVM (`/dev/kvm` opens read-write).
pub fn kvm_usable() -> bool {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// What the plan needs to know about the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostFacts {
    /// `/dev/kvm` is usable.
    pub kvm: bool,
    /// The firmware to load.
    pub firmware: Firmware,
}

/// The accelerator a plan chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedAccel {
    /// KVM.
    Kvm,
    /// TCG.
    Tcg,
}

/// The QEMU command a run starts.
#[derive(Clone, Debug, PartialEq)]
pub struct QemuPlan {
    /// The program to run (QEMU or a wrapper).
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// The accelerator chosen.
    pub accel: ResolvedAccel,
    /// The CPU model.
    pub cpu: String,
    /// The shared guest-RAM file, in trace mode.
    pub shm: Option<PathBuf>,
}

/// Doubles commas, which QEMU option values use as separators.
pub fn escape_value(value: &Path) -> String {
    value.to_string_lossy().replace(',', ",,")
}

/// Plans the QEMU command line for `config`.
///
/// # Errors
///
/// [`Error::KvmUnavailable`] when KVM was requested and is not usable, and
/// [`Error::BootEnvKeysHeadless`] when the boot environment must be typed but the run has no
/// display (no menu appears to type it into).
pub fn plan(
    config: &LaunchConfig,
    run: &RunDir,
    host: &HostFacts,
    with_fw_cfg: bool,
) -> Result<QemuPlan, Error> {
    let accel = match (config.accel, host.kvm) {
        (Accel::Kvm, false) => return Err(Error::KvmUnavailable),
        (Accel::Kvm | Accel::Auto, true) => ResolvedAccel::Kvm,
        (Accel::Tcg, _) | (Accel::Auto, false) => ResolvedAccel::Tcg,
    };
    if config.boot_env_via == BootEnvVia::Keys
        && config.display == Display::None
        && (!config.boot_env.is_empty() || config.mode == Mode::Trace)
    {
        return Err(Error::BootEnvKeysHeadless);
    }
    let cpu = config.cpu.clone().unwrap_or_else(|| match accel {
        ResolvedAccel::Kvm => "host,migratable=off".to_owned(),
        ResolvedAccel::Tcg => "max".to_owned(),
    });
    let shm = config.shm_path();

    let mut args: Vec<String> = config.qemu_wrapper_args.clone();
    let mut push = |items: &[&str]| args.extend(items.iter().map(|s| (*s).to_owned()));

    // debug-threads names the vCPU threads `CPU <n>/KVM`, so host tools can tell them apart.
    push(&[
        "-name",
        &format!("ferric-harness-{},debug-threads=on", run.name()),
    ]);
    let mut machine = "q35".to_owned();
    if config.iommu == Iommu::Vtd {
        machine.push_str(",kernel-irqchip=split");
    }
    if shm.is_some() {
        machine.push_str(",memory-backend=ram");
    }
    push(&["-machine", &machine]);
    match accel {
        ResolvedAccel::Kvm => push(&["-accel", "kvm"]),
        ResolvedAccel::Tcg => push(&["-accel", "tcg"]),
    }
    push(&["-cpu", &cpu]);
    push(&[
        "-smp",
        &config.smp.to_string(),
        "-m",
        &config.mem_mib.to_string(),
    ]);
    if let Some(shm) = &shm {
        push(&[
            "-object",
            &format!(
                "memory-backend-file,id=ram,mem-path={},size={}M,share=on",
                escape_value(shm),
                config.mem_mib
            ),
        ]);
    }
    match &host.firmware {
        Firmware::Bios(path) => push(&["-bios", &path.to_string_lossy()]),
        Firmware::Pflash(path) => push(&[
            "-drive",
            &format!(
                "if=pflash,format=raw,unit=0,readonly=on,file={}",
                escape_value(path)
            ),
        ]),
    }
    // An IOMMU must come before the devices it translates for.
    match config.iommu {
        Iommu::None => {}
        Iommu::Vtd => push(&["-device", "intel-iommu,intremap=on"]),
        Iommu::Amdvi => push(&["-device", "amd-iommu,intremap=off,dma-remap=on"]),
    }
    push(&[
        "-drive",
        &format!(
            "file={},format=raw,if=none,id=drv0,snapshot=on",
            escape_value(&config.image)
        ),
        "-device",
        "nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0",
    ]);
    for (i, disk) in config.extra_disks.iter().enumerate() {
        let id = i + 1;
        push(&[
            "-drive",
            &format!(
                "file={},format=raw,if=none,id=drv{id},snapshot=on",
                escape_value(disk)
            ),
            "-device",
            &format!("nvme,drive=drv{id},serial=NVME_EXTRA{id}"),
        ]);
    }
    push(&[
        "-netdev",
        &format!("user,id=net0,tftp={}", escape_value(&run.tftp_dir())),
    ]);
    match config.nic {
        Nic::E1000 => push(&["-device", "e1000,netdev=net0,addr=5.0"]),
        Nic::Virtio => push(&["-device", "virtio-net-pci,netdev=net0,addr=5.0"]),
    }
    push(&[
        "-device",
        "edu,addr=10.0,dma_mask=0xffffffffffffffff",
        "-device",
        "ich9-intel-hda,id=hda0",
        "-device",
        "hda-output,bus=hda0.0",
        "-device",
        "ich9-intel-hda,id=hda1",
        "-device",
        "hda-output,bus=hda1.0",
    ]);
    let xhci = config.display == Display::Std || config.input == InputDevices::Usb;
    match config.display {
        Display::Std => push(&["-vga", "std", "-display", "none"]),
        Display::None => push(&["-vga", "none", "-display", "none"]),
    }
    if xhci {
        push(&["-device", "qemu-xhci,id=xhci"]);
    }
    if config.input == InputDevices::Usb {
        push(&[
            "-device",
            "usb-tablet,bus=xhci.0",
            "-device",
            "usb-kbd,bus=xhci.0",
        ]);
    }
    if !config.reboot {
        push(&["-no-reboot"]);
    }
    push(&[
        "-qmp",
        &format!("unix:{},server=on,wait=off", escape_value(&run.qmp_sock())),
        "-chardev",
        &format!(
            "socket,id=ser0,path={},server=on,wait=off",
            escape_value(&run.serial_sock())
        ),
        "-serial",
        "chardev:ser0",
        "-monitor",
        "none",
        "-parallel",
        "none",
        "-S",
    ]);
    if with_fw_cfg {
        push(&[
            "-fw_cfg",
            &format!("name={FW_CFG_FILE},file={}", escape_value(&run.boot_env())),
        ]);
    }
    args.extend(config.qemu_args.iter().cloned());

    Ok(QemuPlan {
        program: config.qemu.clone(),
        args: args.into_iter().map(OsString::from).collect(),
        accel,
        cpu,
        shm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostFacts {
        HostFacts {
            kvm: true,
            firmware: Firmware::Bios(PathBuf::from("/fw/OVMF.fd")),
        }
    }

    fn args(plan: &QemuPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn headless_matches_boot_smoke_topology() {
        let run = RunDir::new("/r/smoke,1");
        let mut config = LaunchConfig::new("/img/harddrive.img".into(), "/r/smoke,1".into());
        config.display = Display::None;
        let plan = plan(&config, &run, &host(), false).expect("plan");
        let a = args(&plan);
        assert_eq!(plan.accel, ResolvedAccel::Kvm);
        assert_eq!(value_after(&a, "-cpu"), ["host,migratable=off"]);
        assert_eq!(value_after(&a, "-machine"), ["q35"]);
        let devices = value_after(&a, "-device");
        assert!(devices.contains(&"nvme,drive=drv0,serial=NVME_SERIAL,addr=4.0"));
        assert!(devices.contains(&"e1000,netdev=net0,addr=5.0"));
        assert!(devices.contains(&"edu,addr=10.0,dma_mask=0xffffffffffffffff"));
        assert_eq!(
            devices
                .iter()
                .filter(|d| d.starts_with("ich9-intel-hda"))
                .count(),
            2
        );
        assert!(!devices.iter().any(|d| d.starts_with("qemu-xhci")));
        assert_eq!(value_after(&a, "-vga"), ["none"]);
        assert!(a.contains(&"-no-reboot".to_owned()));
        assert!(a.contains(&"-S".to_owned()));
        assert_eq!(
            value_after(&a, "-drive"),
            ["file=/img/harddrive.img,format=raw,if=none,id=drv0,snapshot=on"]
        );
        // Commas in paths are doubled.
        assert_eq!(
            value_after(&a, "-netdev"),
            ["user,id=net0,tftp=/r/smoke,,1/tftp"]
        );
        assert_eq!(
            value_after(&a, "-qmp"),
            ["unix:/r/smoke,,1/qmp.sock,server=on,wait=off"]
        );
        assert!(value_after(&a, "-fw_cfg").is_empty());
        assert!(plan.shm.is_none());
    }

    #[test]
    fn display_trace_and_options() {
        let run = RunDir::new("/r/w");
        let mut config = LaunchConfig::new("/img/a.img".into(), "/r/w".into());
        config.mode = Mode::Trace;
        config.input = InputDevices::Usb;
        config.iommu = Iommu::Vtd;
        config.nic = Nic::Virtio;
        config.mem_mib = 4096;
        config.reboot = true;
        config.extra_disks = vec!["/img/extra.img".into()];
        config.qemu_wrapper_args = vec!["-c".into(), "0-3".into(), "qemu".into()];
        config.qemu_args = vec!["-d".into(), "guest_errors".into()];
        let plan = plan(&config, &run, &host(), true).expect("plan");
        let a = args(&plan);
        assert_eq!(&a[..3], ["-c", "0-3", "qemu"]);
        assert_eq!(&a[a.len() - 2..], ["-d", "guest_errors"]);
        assert_eq!(
            value_after(&a, "-machine"),
            ["q35,kernel-irqchip=split,memory-backend=ram"]
        );
        assert_eq!(
            value_after(&a, "-object"),
            ["memory-backend-file,id=ram,mem-path=/dev/shm/ferric-harness-w,size=4096M,share=on"]
        );
        assert_eq!(plan.shm, Some(PathBuf::from("/dev/shm/ferric-harness-w")));
        let devices = value_after(&a, "-device");
        assert_eq!(devices[0], "intel-iommu,intremap=on");
        assert!(devices.contains(&"virtio-net-pci,netdev=net0,addr=5.0"));
        assert!(devices.contains(&"qemu-xhci,id=xhci"));
        assert!(devices.contains(&"usb-tablet,bus=xhci.0"));
        assert!(devices.contains(&"nvme,drive=drv1,serial=NVME_EXTRA1"));
        assert_eq!(value_after(&a, "-vga"), ["std"]);
        assert!(!a.contains(&"-no-reboot".to_owned()));
        assert_eq!(
            value_after(&a, "-fw_cfg"),
            ["name=opt/org.ferric-oxide/boot-env,file=/r/w/boot-env"]
        );
    }

    #[test]
    fn accelerator_choice() {
        let run = RunDir::new("/r/x");
        let mut config = LaunchConfig::new("/i".into(), "/r/x".into());
        let no_kvm = HostFacts {
            kvm: false,
            ..host()
        };
        let p = plan(&config, &run, &no_kvm, false).expect("plan");
        assert_eq!((p.accel, p.cpu.as_str()), (ResolvedAccel::Tcg, "max"));
        config.accel = Accel::Kvm;
        assert!(matches!(
            plan(&config, &run, &no_kvm, false),
            Err(Error::KvmUnavailable)
        ));
        config.accel = Accel::Tcg;
        config.cpu = Some("Skylake-Server".into());
        let p = plan(&config, &run, &host(), false).expect("plan");
        assert_eq!(
            (p.accel, p.cpu.as_str()),
            (ResolvedAccel::Tcg, "Skylake-Server")
        );
        let pflash = HostFacts {
            kvm: true,
            firmware: Firmware::Pflash("/fw/code.fd".into()),
        };
        let a = args(&plan(&config, &run, &pflash, false).expect("plan"));
        assert!(
            value_after(&a, "-drive")
                .contains(&"if=pflash,format=raw,unit=0,readonly=on,file=/fw/code.fd")
        );
    }

    #[test]
    fn typed_boot_env_needs_a_display() {
        let run = RunDir::new("/r/x");
        let mut config = LaunchConfig::new("/i".into(), "/r/x".into());
        config.display = Display::None;
        config.boot_env_via = BootEnvVia::Keys;
        config.boot_env = vec![("A".into(), "1".into())];
        assert!(matches!(
            plan(&config, &run, &host(), false),
            Err(Error::BootEnvKeysHeadless)
        ));
    }

    #[test]
    fn firmware_search() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fw = tmp.path().join("fw.fd");
        std::fs::write(&fw, b"").expect("write");
        assert_eq!(find_firmware(Some(&fw)).expect("found"), Firmware::Bios(fw));
        assert!(matches!(
            find_firmware(Some(&tmp.path().join("missing"))),
            Err(Error::NoFirmware { .. })
        ));
    }
}
