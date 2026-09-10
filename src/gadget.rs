//! Brings up the AOA interface on a FunctionFS instance, in one of two modes:
//!
//!   [`FunctionFsGadget::setup`]  - self-contained. Composes the whole gadget in
//!              configfs, mounts FunctionFS, writes the descriptors and binds the
//!              UDC. Undone completely on teardown.
//!
//!   [`FunctionFsGadget::attach`] - cooperative. Somebody else (an init script,
//!              an existing multi-function gadget) composed the gadget and
//!              mounted FunctionFS; we only write the descriptors, and optionally
//!              bind/unbind a named configfs gadget around the run.
//!
//! Either way the endpoint files and the EP0 event stream are reached the same
//! way, so the capture runtime does not care which mode it is running in.

use std::ffi::CString;
use std::ptr;

use crate::common::{errno, log, strerror, Fd};

const CONFIGFS: &str = "/sys/kernel/config";
const USB_GADGET_DIR: &str = "/sys/kernel/config/usb_gadget";

// Placeholders: FunctionFS renumbers the interface and the UDC's autoconfig
// assigns the real endpoint addresses.
const EP_IN: u8 = 0x81; // IN = commands
const EP_OUT: u8 = 0x02; // OUT = video
const LANGID: u16 = 0x0409; // en-US

// --- FunctionFS ABI ---------------------------------------------------------
const FUNCTIONFS_DESCRIPTORS_MAGIC_V2: u32 = 3;
const FUNCTIONFS_STRINGS_MAGIC: u32 = 2;
const FUNCTIONFS_HAS_FS_DESC: u32 = 1;
const FUNCTIONFS_HAS_HS_DESC: u32 = 2;

/// `_IOR('g', 130, struct usb_endpoint_descriptor)` -- the 9-byte packed struct.
const FUNCTIONFS_ENDPOINT_DESC: libc::c_ulong = 0x8009_6782;

/// `struct usb_functionfs_event` is a packed 8-byte `usb_ctrlrequest` union plus
/// a `type` byte and 3 pad bytes.
pub const FFS_EVENT_SIZE: usize = 12;

pub const FUNCTIONFS_UNBIND: u8 = 1;
pub const FUNCTIONFS_ENABLE: u8 = 2;
pub const FUNCTIONFS_DISABLE: u8 = 3;
pub const FUNCTIONFS_SETUP: u8 = 4;

// --- the AOA identity ------------------------------------------------------

/// The AOA identity the goggles validate before they will open the video path.
pub struct GadgetIdentity {
    pub vid: u16,
    pub pid: u16,
    pub manufacturer: String,  // iManufacturer
    pub product: String,       // iProduct
    pub serial: String,        // iSerialNumber
    pub configuration: String, // iConfiguration
    pub interface_name: String, // iInterface
}

// --- tiny sysfs/configfs helpers -----------------------------------------------

fn cstr(s: &str) -> CString {
    CString::new(s).expect("path contains a NUL byte")
}

fn write_file(path: &str, value: &str) -> bool {
    let cpath = cstr(path);
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_WRONLY) };
    if fd < 0 {
        return false;
    }
    let n = unsafe {
        libc::write(
            fd,
            value.as_ptr() as *const libc::c_void,
            value.len(),
        )
    };
    unsafe { libc::close(fd) };
    n == value.len() as isize
}

fn read_file(path: &str) -> String {
    let cpath = cstr(path);
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return String::new();
    }
    let mut buf = [0u8; 256];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len() - 1) };
    unsafe { libc::close(fd) };
    if n <= 0 {
        return String::new();
    }
    let mut s = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
    while matches!(s.chars().next_back(), Some('\n') | Some(' ')) {
        s.pop();
    }
    s
}

// configfs rejects a bad attribute at write() time, so a silent failure here
// would show up much later as a gadget the goggles refuse to talk to.
fn set_attr(path: &str, value: &str) -> bool {
    if write_file(path, value) {
        return true;
    }
    log(format!("cannot set {} = '{}'", path, value));
    false
}

fn make_dir(path: &str) -> bool {
    let r = unsafe { libc::mkdir(cstr(path).as_ptr(), 0o755) };
    r == 0 || errno() == libc::EEXIST
}

// The gadget core finishes releasing a function asynchronously, so a directory
// can still be busy for a moment after the last thing referencing it is gone.
fn remove_dir(path: &str) -> bool {
    let cpath = cstr(path);
    for _ in 0..20 {
        let r = unsafe { libc::rmdir(cpath.as_ptr()) };
        if r == 0 || errno() == libc::ENOENT {
            return true;
        }
        unsafe { libc::usleep(50_000) };
    }
    log(format!("cannot remove {}", path));
    false
}

fn list_dir(path: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                names.push(name);
            }
        }
    }
    names
}

fn path_exists(path: &str) -> bool {
    unsafe { libc::access(cstr(path).as_ptr(), libc::F_OK) == 0 }
}

fn ensure_configfs() -> bool {
    if path_exists(USB_GADGET_DIR) {
        return true;
    }
    let src = cstr("none");
    let target = cstr(CONFIGFS);
    let fstype = cstr("configfs");
    unsafe {
        libc::mount(
            src.as_ptr(),
            target.as_ptr(),
            fstype.as_ptr(),
            0,
            ptr::null(),
        );
    }
    if path_exists(USB_GADGET_DIR) {
        return true;
    }
    log(format!(
        "no {} - is CONFIG_USB_CONFIGFS enabled?",
        USB_GADGET_DIR
    ));
    false
}

// --- FunctionFS descriptor blocks --------------------------------------------

fn add_u32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

fn add_endpoint(v: &mut Vec<u8>, addr: u8, max_packet: u16) {
    v.extend_from_slice(&[
        7,
        5,
        addr,
        0x02, // bulk
        (max_packet & 0xFF) as u8,
        (max_packet >> 8) as u8,
        0,
    ]);
}

// One vendor-specific interface with EP1 IN (commands) and EP2 OUT (video).
fn interface_block(max_packet: u16) -> Vec<u8> {
    let mut v: Vec<u8> = vec![9, 4, 0, 0, 2, 0xFF, 0xFF, 0, 1]; // iInterface = 1
    add_endpoint(&mut v, EP_IN, max_packet);
    add_endpoint(&mut v, EP_OUT, max_packet);
    v
}

pub fn ffs_descriptors() -> Vec<u8> {
    let fs = interface_block(64);
    let hs = interface_block(512);

    let mut out = Vec::new();
    add_u32(&mut out, FUNCTIONFS_DESCRIPTORS_MAGIC_V2);
    add_u32(&mut out, 0); // length, patched below
    add_u32(&mut out, FUNCTIONFS_HAS_FS_DESC | FUNCTIONFS_HAS_HS_DESC);
    add_u32(&mut out, 3); // fs descriptor count
    add_u32(&mut out, 3); // hs descriptor count
    out.extend_from_slice(&fs);
    out.extend_from_slice(&hs);

    let len = out.len() as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

pub fn ffs_strings(interface_name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    add_u32(&mut out, FUNCTIONFS_STRINGS_MAGIC);
    add_u32(&mut out, 0); // length, patched below
    add_u32(&mut out, 1); // one string
    add_u32(&mut out, 1); // one language
    out.push((LANGID & 0xFF) as u8);
    out.push((LANGID >> 8) as u8);
    out.extend_from_slice(interface_name.as_bytes());
    out.push(0);

    let len = out.len() as u32;
    out[4..8].copy_from_slice(&len.to_le_bytes());
    out
}

// --- gadget lifecycle --------------------------------------------------------

pub struct FunctionFsGadget {
    name: String,
    udc: String,
    mount_point: String,
    gadget_path: String,
    prev_gadget: String, // whose UDC we took; restored on teardown
    ep0_fd: libc::c_int,
    mounted: bool,  // we mounted it
    composed: bool, // we built the configfs tree
    bound: bool,    // we bound the UDC
}

fn udc_file_of(gadget: &str) -> String {
    format!("{}/{}/UDC", USB_GADGET_DIR, gadget)
}

impl FunctionFsGadget {
    pub fn new(name: String, udc: String) -> FunctionFsGadget {
        let mount_point = format!("/dev/ffs-{}", name);
        let gadget_path = format!("{}/{}", USB_GADGET_DIR, name);
        FunctionFsGadget {
            name,
            udc,
            mount_point,
            gadget_path,
            prev_gadget: String::new(),
            ep0_fd: -1,
            mounted: false,
            composed: false,
            bound: false,
        }
    }

    /// Override the default mount point (`/dev/ffs-<name>`). Self-composed mode.
    pub fn set_mount_point(&mut self, path: String) {
        self.mount_point = path;
    }

    pub fn ep0(&self) -> libc::c_int {
        self.ep0_fd
    }

    /// Every USB device controller the board exposes, in `/sys/class/udc` order.
    pub fn list_udcs() -> Vec<String> {
        let mut udcs = list_dir("/sys/class/udc");
        udcs.sort();
        udcs
    }

    // Only one gadget may own a UDC. Park whoever has it -- on a stock Rockchip
    // image that is the vendor gadget running adb -- and remember the name so
    // teardown() can hand it back.
    fn release_udc(&mut self) -> bool {
        for g in list_dir(USB_GADGET_DIR) {
            if g == self.name || read_file(&udc_file_of(&g)) != self.udc {
                continue;
            }
            log(format!("releasing UDC from gadget '{}'", g));
            if !write_file(&udc_file_of(&g), "\n") {
                log(format!(
                    "cannot unbind gadget '{}' from {}",
                    g, self.udc
                ));
                return false;
            }
            self.prev_gadget = g;
        }
        true
    }

    // Only ever hands the UDC to the gadget release_udc() took it from. A run
    // that took nothing (attach mode, or a free controller) restores nothing.
    fn restore_udc(&mut self) -> bool {
        if self.prev_gadget.is_empty() || self.udc.is_empty() {
            return true;
        }
        let ok = write_file(&udc_file_of(&self.prev_gadget), &self.udc);
        log(if ok {
            format!("UDC handed back to gadget '{}'", self.prev_gadget)
        } else {
            format!(
                "could not hand the UDC back to gadget '{}'",
                self.prev_gadget
            )
        });
        self.prev_gadget.clear();
        ok
    }

    // Build the configfs tree: device descriptor, identity strings, one
    // configuration, and the FunctionFS function linked into it.
    fn compose(&mut self, id: &GadgetIdentity) -> bool {
        let g = self.gadget_path.clone();

        if !make_dir(&g) {
            log(format!("cannot create {}", g));
            return false;
        }
        self.composed = true; // from here on teardown must run

        let mut ok = true;
        ok &= set_attr(&format!("{}/idVendor", g), &format!("0x{:04x}", id.vid));
        ok &= set_attr(&format!("{}/idProduct", g), &format!("0x{:04x}", id.pid));
        ok &= set_attr(&format!("{}/bcdUSB", g), "0x0200"); // USB 2.0
        ok &= set_attr(&format!("{}/bcdDevice", g), "0x0200");
        ok &= set_attr(&format!("{}/bDeviceClass", g), "0x00");
        ok &= set_attr(&format!("{}/bDeviceSubClass", g), "0x00");
        ok &= set_attr(&format!("{}/bDeviceProtocol", g), "0x00");
        ok &= set_attr(&format!("{}/bMaxPacketSize0", g), "64");
        // The AOA descriptors describe a USB 2.0 device; keep a SuperSpeed-capable
        // controller from advertising a speed we ship no descriptors for.
        ok &= set_attr(&format!("{}/max_speed", g), "high-speed");

        if !make_dir(&format!("{}/strings/0x409", g)) {
            log("cannot create device strings");
            return false;
        }
        ok &= set_attr(&format!("{}/strings/0x409/manufacturer", g), &id.manufacturer);
        ok &= set_attr(&format!("{}/strings/0x409/product", g), &id.product);
        ok &= set_attr(&format!("{}/strings/0x409/serialnumber", g), &id.serial);

        if !make_dir(&format!("{}/configs/c.1", g))
            || !make_dir(&format!("{}/configs/c.1/strings/0x409", g))
        {
            log("cannot create configuration");
            return false;
        }
        ok &= set_attr(
            &format!("{}/configs/c.1/strings/0x409/configuration", g),
            &id.configuration,
        );
        ok &= set_attr(&format!("{}/configs/c.1/bmAttributes", g), "0xc0"); // self-powered
        ok &= set_attr(&format!("{}/configs/c.1/MaxPower", g), "2"); // -> bMaxPower = 1
        if !ok {
            log("the AOA identity is incomplete - the goggles would reject it");
            return false;
        }

        let fn_name = format!("ffs.{}", self.name);
        if !make_dir(&format!("{}/functions/{}", g, fn_name)) {
            log(format!("cannot create {}", fn_name));
            return false;
        }
        let target = format!("{}/functions/{}", g, fn_name);
        let linkpath = format!("{}/configs/c.1/{}", g, fn_name);
        let r = unsafe { libc::symlink(cstr(&target).as_ptr(), cstr(&linkpath).as_ptr()) };
        if r != 0 && errno() != libc::EEXIST {
            log(format!("cannot link {} into the configuration", fn_name));
            return false;
        }

        if !make_dir(&self.mount_point) {
            log(format!("cannot create {}", self.mount_point));
            return false;
        }
        // The mount's source name is what pairs it with functions/ffs.<name>.
        let src = cstr(&self.name);
        let mp = cstr(&self.mount_point);
        let fstype = cstr("functionfs");
        if unsafe {
            libc::mount(src.as_ptr(), mp.as_ptr(), fstype.as_ptr(), 0, ptr::null())
        } != 0
        {
            log(format!(
                "mount functionfs on {}: {}",
                self.mount_point,
                strerror(errno())
            ));
            return false;
        }
        self.mounted = true;
        true
    }

    // Writing the descriptors + strings to ep0 is what makes the function (and
    // the ep1/ep2 files) real; the gadget cannot bind before this.
    fn write_descriptors(&mut self, id: &GadgetIdentity) -> bool {
        let ep0_path = format!("{}/ep0", self.mount_point);
        self.ep0_fd = unsafe { libc::open(cstr(&ep0_path).as_ptr(), libc::O_RDWR) };
        if self.ep0_fd < 0 {
            log(format!("open {}: {}", ep0_path, strerror(errno())));
            return false;
        }
        let desc = ffs_descriptors();
        if unsafe {
            libc::write(
                self.ep0_fd,
                desc.as_ptr() as *const libc::c_void,
                desc.len(),
            )
        } < 0
        {
            log(format!("write descriptors: {}", strerror(errno())));
            return false;
        }
        let s = ffs_strings(&id.interface_name);
        if unsafe {
            libc::write(self.ep0_fd, s.as_ptr() as *const libc::c_void, s.len())
        } < 0
        {
            log(format!("write strings: {}", strerror(errno())));
            return false;
        }
        true
    }

    fn bind(&mut self) -> bool {
        if self.udc.is_empty() {
            log(format!("no UDC to bind {} to", self.gadget_path));
            return false;
        }
        if !self.release_udc() {
            return false;
        }
        if !write_file(&format!("{}/UDC", self.gadget_path), &self.udc) {
            log(format!(
                "cannot bind {} to UDC {}",
                self.gadget_path, self.udc
            ));
            return false;
        }
        self.bound = true;

        // composite_bind() bumps bcdUSB to 0x0210 to advertise LPM/BOS. The AOA
        // accessory the goggles expect is a plain USB 2.0 device, so put it back;
        // the attribute is the descriptor the host is served, and stays writable
        // while bound.
        write_file(&format!("{}/bcdUSB", self.gadget_path), "0x0200");
        let bcd = read_file(&format!("{}/bcdUSB", self.gadget_path));
        if bcd != "0x0200" {
            log(format!("warning: the kernel holds bcdUSB at {}", bcd));
        }

        log(format!("gadget '{}' bound to {}", self.name, self.udc));
        true
    }

    // --- mode 1: compose everything ourselves --------------------------------
    pub fn setup(&mut self, id: &GadgetIdentity) -> bool {
        if !ensure_configfs() {
            return false;
        }
        if path_exists(&self.gadget_path) {
            log(format!(
                "gadget '{}' already exists - run with --cleanup first",
                self.name
            ));
            return false;
        }
        self.compose(id) && self.write_descriptors(id) && self.bind()
    }

    // --- mode 2: use a gadget somebody else composed -----------------------
    pub fn attach(&mut self, ffs_path: &str, gadget_dir: &str, id: &GadgetIdentity) -> bool {
        if !path_exists(&format!("{}/ep0", ffs_path)) {
            log(format!(
                "no ep0 under {} - is FunctionFS mounted there?",
                ffs_path
            ));
            return false;
        }
        self.mount_point = ffs_path.to_string(); // not ours: teardown will not unmount
        log(format!("using the FunctionFS instance at {}", ffs_path));

        if !self.write_descriptors(id) {
            return false;
        }

        if gadget_dir.is_empty() {
            log("descriptors written; the UDC is somebody else's to bind");
            return true;
        }
        if !path_exists(gadget_dir) {
            log(format!("no such gadget: {}", gadget_dir));
            return false;
        }
        self.gadget_path = gadget_dir.to_string();
        self.bind()
    }

    /// `"ep1"` (IN, commands) / `"ep2"` (OUT, video); -1 on failure.
    pub fn open_endpoint(&self, name: &str) -> libc::c_int {
        let path = format!("{}/{}", self.mount_point, name);
        let fd = unsafe { libc::open(cstr(&path).as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            return -1;
        }
        // The controller's autoconfig assigns the real addresses and rewrites the
        // descriptor the host is served, so report what the goggles will actually
        // see rather than what we asked for.
        let mut d = [0u8; 9];
        if unsafe { libc::ioctl(fd, FUNCTIONFS_ENDPOINT_DESC, d.as_mut_ptr()) } == 0 {
            let address = d[2];
            let max_packet = u16::from_le_bytes([d[4], d[5]]);
            log(format!(
                "{} -> address 0x{:02x}, wMaxPacketSize {}",
                name, address, max_packet
            ));
        }
        fd
    }

    pub fn teardown(&mut self) {
        if !self.composed && !self.bound && self.ep0_fd < 0 {
            return;
        }

        if self.bound {
            write_file(&format!("{}/UDC", self.gadget_path), "\n");
            self.bound = false;
            unsafe { libc::usleep(200_000) }; // let in-flight transfers retire
        }
        if self.ep0_fd >= 0 {
            unsafe { libc::close(self.ep0_fd) };
            self.ep0_fd = -1;
        }

        if !self.composed {
            // attached mode: nothing else is ours
            self.restore_udc();
            return;
        }

        if self.mounted {
            // Lazy on purpose. A plain umount() of this mount can park the
            // process in uninterruptible sleep inside generic_shutdown_super(),
            // waiting on the superblock's writeback workqueue -- at which point
            // not even SIGKILL gets it back. MNT_DETACH returns immediately and
            // lets the kernel release the superblock once its last reference
            // goes away.
            unsafe { libc::umount2(cstr(&self.mount_point).as_ptr(), libc::MNT_DETACH) };
            self.mounted = false;
        }
        unsafe { libc::rmdir(cstr(&self.mount_point).as_ptr()) };

        let g = self.gadget_path.clone();
        let fn_name = format!("ffs.{}", self.name);
        unsafe { libc::unlink(cstr(&format!("{}/configs/c.1/{}", g, fn_name)).as_ptr()) };
        remove_dir(&format!("{}/configs/c.1/strings/0x409", g));
        remove_dir(&format!("{}/configs/c.1", g));
        remove_dir(&format!("{}/functions/{}", g, fn_name));
        remove_dir(&format!("{}/strings/0x409", g));
        remove_dir(&g);

        self.composed = false;
        self.restore_udc();
    }

    // Remove a gadget left behind by a killed run and hand the UDC (empty =>
    // none available) back to whichever other gadget is left unbound.
    pub fn cleanup_stale(name: &str, mount_point: &str, udc: &str) -> bool {
        if !ensure_configfs() {
            return false;
        }

        let mut stale = FunctionFsGadget::new(name.to_string(), udc.to_string());
        if !mount_point.is_empty() {
            stale.mount_point = mount_point.to_string();
        }

        if !path_exists(&stale.gadget_path) && !path_exists(&stale.mount_point) {
            log("nothing to clean up");
            return true;
        }
        stale.composed = true;
        stale.mounted = true; // the unmount is harmless if it is not
        stale.bound = true;
        stale.prev_gadget = guess_previous_owner(name);
        stale.teardown();
        log(format!("cleaned up gadget '{}'", name));
        true
    }
}

impl Drop for FunctionFsGadget {
    fn drop(&mut self) {
        self.teardown();
    }
}

// The killed run took its record of the previous owner with it, so guess: the
// first other gadget that is left without a controller.
fn guess_previous_owner(our_name: &str) -> String {
    for g in list_dir(USB_GADGET_DIR) {
        if g != our_name && read_file(&udc_file_of(&g)).is_empty() {
            return g;
        }
    }
    String::new()
}

/// A `dup()` of stdout, or a freshly created/truncated file.
pub fn open_sink(path: &str) -> Fd {
    if path == "stdout" {
        return Fd::new(unsafe { libc::dup(libc::STDOUT_FILENO) });
    }
    let fd = unsafe {
        libc::open(
            cstr(path).as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
            0o644,
        )
    };
    Fd::new(fd)
}
