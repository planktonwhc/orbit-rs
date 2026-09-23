//! Find and bring up the RNDIS network interface the goggles enumerate as
//! once "Liveview sharing" is turned on, plugged into a *regular* USB port
//! (not the gadget/OTG port `capture.rs`'s AOA path uses). Ported verbatim
//! from `../../goggles-net/src/iface.rs` -- see `handshake.rs`'s doc
//! comment for why this port needed no logic changes.
//!
//! The USB parent's idVendor:idProduct (0x2ca3:0x0020) is the one concrete
//! fact recovered from the reference tool's strings ("RNDIS interface for
//! goggles-net mode. Omit to auto-detect the 2ca3:0020 USB parent"), so
//! that's what auto-detect matches on rather than guessing an interface name
//! (`usb0`/`eth1`/... varies by kernel and by what else is plugged in).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::common::log;

const NET_CLASS: &str = "/sys/class/net";

fn read_trimmed(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// sysfs idVendor/idProduct files are plain hex with no "0x" prefix
/// ("2ca3\n") -- accept one anyway, it costs nothing.
fn parse_hex_id(s: &str) -> Option<u16> {
    u16::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

fn matches_ids(dev_dir: &Path, vendor: u16, product: u16) -> bool {
    let v = read_trimmed(&dev_dir.join("idVendor")).and_then(|s| parse_hex_id(&s));
    let p = read_trimmed(&dev_dir.join("idProduct")).and_then(|s| parse_hex_id(&s));
    v == Some(vendor) && p == Some(product)
}

/// `/sys/class/net/<iface>/device` is a symlink into the USB *interface*
/// node (e.g. `.../usb1/1-1/1-1:1.0`); idVendor/idProduct live on the
/// *device* node one or more levels up, and exactly how many depends on
/// whether the goggles present as a simple or a composite USB device -- so
/// walk upward a bounded number of levels instead of assuming a fixed depth.
fn usb_parent_matches(iface_device_link: &Path, vendor: u16, product: u16) -> bool {
    let mut dir = match fs::canonicalize(iface_device_link) {
        Ok(d) => d,
        Err(_) => return false,
    };
    for _ in 0..6 {
        if matches_ids(&dir, vendor, product) {
            return true;
        }
        match dir.parent() {
            Some(p) => dir = p.to_path_buf(),
            None => break,
        }
    }
    false
}

/// One pass over `/sys/class/net/*` for an interface whose USB parent is
/// `vendor:product`. Entries with no `device` symlink (lo, veth, the Wi-Fi
/// AP's own interfaces, ...) are skipped up front.
pub fn find_by_usb_ids(vendor: u16, product: u16) -> Option<String> {
    let rd = fs::read_dir(NET_CLASS).ok()?;
    for ent in rd.flatten() {
        let dev_link = ent.path().join("device");
        if !dev_link.exists() {
            continue;
        }
        if usb_parent_matches(&dev_link, vendor, product) {
            return Some(ent.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

/// The USB *device* sysfs dir (the one with `idVendor`/`idProduct`, not the
/// interface node) for a given netdev name -- same upward walk as
/// [`usb_parent_matches`], just returning the path instead of a bool. Used
/// to reach `power/control` for [`disable_autosuspend`].
fn usb_device_dir(iface: &str) -> Option<PathBuf> {
    let mut dir = fs::canonicalize(Path::new(NET_CLASS).join(iface).join("device")).ok()?;
    for _ in 0..6 {
        if dir.join("idVendor").exists() && dir.join("idProduct").exists() {
            return Some(dir);
        }
        dir = dir.parent()?.to_path_buf();
    }
    None
}

/// Write "on" to the USB device's `power/control`, disabling autosuspend.
///
/// Confirmed relevant by `dji-rs`'s own strings ("goggles-net: cannot
/// disable autosuspend at ..."): it does this defensively for the same
/// RNDIS link, which makes sense -- an idle-looking USB network device is a
/// completely ordinary autosuspend candidate to the kernel, and getting
/// suspended mid-session would look identical to every other "the goggles
/// just stopped sending" symptom already ruled out. Best-effort: some
/// kernels/devices don't expose `power/control` as writable, or don't need
/// it (autosuspend already disabled globally) -- logged, not fatal.
fn disable_autosuspend(iface: &str) {
    let Some(dev_dir) = usb_device_dir(iface) else {
        log(format!("goggles-net: could not locate {iface}'s USB device dir to disable autosuspend"));
        return;
    };
    let control = dev_dir.join("power").join("control");
    match fs::write(&control, b"on") {
        Ok(()) => log(format!("goggles-net: disabled USB autosuspend at {}", control.display())),
        Err(e) => log(format!(
            "goggles-net: could not disable USB autosuspend at {}: {e} (continuing)",
            control.display()
        )),
    }
}

/// Poll for the interface to appear. Expected to wait, not an error path --
/// the RNDIS device only shows up after the operator plugs in the goggles
/// *and* toggles Liveview sharing in their menu, which happens after this
/// process has already started.
pub fn wait_for(vendor: u16, product: u16, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut told = false;
    loop {
        if let Some(name) = find_by_usb_ids(vendor, product) {
            return Some(name);
        }
        if !told {
            log(format!(
                "goggles-net: waiting for the {vendor:04x}:{product:04x} RNDIS interface -- \
                 plug the goggles into a REGULAR USB port (not the gadget/OTG port) and turn \
                 on Liveview sharing"
            ));
            told = true;
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(Duration::from_millis(500));
    }
}

/// Tell NetworkManager to leave this interface alone.
///
/// Confirmed by a real capture (`tcpdump -i usb0`): as soon as the RNDIS
/// interface appears, NetworkManager claims it like any other new link and
/// starts its own DHCP client on it (repeating `BOOTP/DHCP Request` every
/// few seconds, never answered -- the goggles aren't a DHCP server). Worse
/// than just noise: NM's own address/state reconciliation was very likely
/// what wiped the static address [`bring_up`] had just assigned, which is
/// why no goggles traffic was ever received even though the interface,
/// idVendor:idProduct match, and `ip addr add` all looked fine in isolation.
/// `nmcli device set <iface> managed no` hands the interface fully to us --
/// same shell-out-to-nmcli tradeoff orbit-net already makes, just pointed
/// the other way (telling NM to back OFF a device instead of configuring
/// one). Best-effort: nmcli missing or NM not yet aware of the interface is
/// logged, not fatal -- `bring_up` proceeds either way.
fn disable_nm_management(iface: &str) {
    if let Err(e) = run("nmcli", &["device", "set", iface, "managed", "no"]) {
        log(format!(
            "goggles-net: could not tell NetworkManager to ignore {iface}: {e} \
             (continuing -- if NM is running, it may fight over this interface's address)"
        ));
    }
}

/// `ip link set <iface> up` + `ip addr add <cidr> dev <iface>`, after asking
/// NetworkManager to step aside (see [`disable_nm_management`]). Shells out
/// rather than hand-rolling netlink -- the same tradeoff orbit-net already
/// makes with nmcli. An address that's already assigned is not an error
/// (logged and ignored, not propagated), since re-running this is normal
/// (the goggles can re-enumerate mid-session).
pub fn bring_up(iface: &str, cidr: &str) -> io::Result<()> {
    disable_nm_management(iface);
    disable_autosuspend(iface);
    run("ip", &["link", "set", iface, "up"])?;
    if let Err(e) = run("ip", &["addr", "add", cidr, "dev", iface]) {
        log(format!("goggles-net: ip addr add {cidr} dev {iface}: {e} (continuing -- may already be set)"));
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> io::Result<()> {
    let status = std::process::Command::new(cmd).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{cmd} {} failed: {status}", args.join(" ")),
        ))
    }
}
