use anyhow::{bail, Result};
use libc::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ffi::{CStr, CString};
use std::io;
use std::mem::size_of;
use std::process::Command;

const IFNAMSIZ: usize = 16;
const BRDGADD: c_ulong = 0;
const BRDGDEL: c_ulong = 1;
const IOC_IN: c_ulong = 0x8000_0000;
const IOC_OUT: c_ulong = 0x4000_0000;

const fn ioc(direction: c_ulong, number: c_ulong, size: usize) -> c_ulong {
    direction | ((size as c_ulong) << 16) | (('i' as c_ulong) << 8) | number
}

const SIOCSIFFLAGS: c_ulong = ioc(IOC_IN, 16, size_of::<libc::ifreq>());
const SIOCGIFFLAGS: c_ulong = ioc(IOC_IN | IOC_OUT, 17, size_of::<libc::ifreq>());
const SIOCAIFADDR: c_ulong = ioc(IOC_IN, 43, 68);
const SIOCAIFADDR_IN6: c_ulong = ioc(IOC_IN, 27, 136);
const SIOCSIFNAME: c_ulong = ioc(IOC_IN, 40, size_of::<libc::ifreq>());
const SIOCIFDESTROY: c_ulong = ioc(IOC_IN, 121, size_of::<libc::ifreq>());
const SIOCIFCREATE2: c_ulong = ioc(IOC_IN | IOC_OUT, 124, size_of::<libc::ifreq>());
const SIOCAIFGROUP: c_ulong = ioc(IOC_IN, 135, size_of::<Ifgroupreq>());
const SIOCSDRVSPEC: c_ulong = ioc(IOC_IN, 123, size_of::<libc::ifdrv>());
const SIOCSIFVNET: c_ulong = ioc(IOC_IN | IOC_OUT, 90, size_of::<libc::ifreq>());

#[repr(C)]
union IfgroupUnion {
    group: [c_char; IFNAMSIZ],
    groups: *mut c_void,
}

#[repr(C)]
struct Ifgroupreq {
    name: [c_char; IFNAMSIZ],
    len: c_uint,
    data: IfgroupUnion,
}

#[repr(C)]
struct Ifbreq {
    member: [c_char; IFNAMSIZ],
    ifs_flags: u32,
    stp_flags: u32,
    path_cost: u32,
    port_no: u8,
    priority: u8,
    protocol: u8,
    role: u8,
    state: u8,
    addr_count: u32,
    addr_max: u32,
    addr_exceeded: u32,
    pvid: u16,
    vlan_protocol: u16,
    pad: [u8; 28],
}

#[repr(C)]
struct BsdSockaddr {
    len: u8,
    family: u8,
    data: [i8; 14],
}

#[repr(C)]
struct Ifaliasreq {
    name: [c_char; IFNAMSIZ],
    address: BsdSockaddr,
    broadcast: BsdSockaddr,
    mask: BsdSockaddr,
    vhid: i32,
}

#[repr(C)]
struct BsdSockaddrIn6 {
    len: u8,
    family: u8,
    port: u16,
    flowinfo: u32,
    address: [u8; 16],
    scope_id: u32,
}

#[repr(C)]
struct In6AddrLifetime {
    expire: libc::time_t,
    preferred: libc::time_t,
    valid: u32,
    preferred_lifetime: u32,
}

#[repr(C)]
struct In6Aliasreq {
    name: [c_char; IFNAMSIZ],
    address: BsdSockaddrIn6,
    destination: BsdSockaddrIn6,
    prefix_mask: BsdSockaddrIn6,
    flags: i32,
    lifetime: In6AddrLifetime,
    vhid: i32,
}

fn interface_name(destination: &mut [c_char; IFNAMSIZ], name: &str) -> Result<()> {
    let name = CString::new(name)?;
    let bytes = name.as_bytes_with_nul();
    if bytes.len() > destination.len() {
        bail!("interface name is too long");
    }
    for (destination, source) in destination.iter_mut().zip(bytes) {
        *destination = *source as c_char;
    }
    Ok(())
}

fn read_interface_name(name: &[c_char; IFNAMSIZ]) -> Result<String> {
    let length = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf8(
        name[..length].iter().map(|byte| *byte as u8).collect(),
    )?)
}

fn control_socket() -> Result<c_int> {
    control_socket_family(libc::AF_LOCAL)
}

fn control_socket_family(family: c_int) -> Result<c_int> {
    let fd = unsafe { libc::socket(family, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(fd)
    }
}

fn ioctl(fd: c_int, request: c_ulong, argument: *mut c_void, operation: &str) -> Result<()> {
    if unsafe { libc::ioctl(fd, request, argument) } < 0 {
        bail!("{operation}: {}", io::Error::last_os_error());
    }
    Ok(())
}

pub fn create(kind: &str) -> Result<String> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, kind)?;
    let result = unsafe { libc::ioctl(fd, SIOCIFCREATE2, &mut request as *mut _ as *mut c_void) };
    let result = if result < 0 {
        if let Some(module) = interface_module(kind) {
            let loaded = matches!(
                Command::new("kldstat")
                    .args(["-q", "-n", module])
                    .status(),
                Ok(status) if status.success()
            );
            if !loaded {
                let _ = Command::new("kldload").arg(module).status();
            }
            unsafe { libc::ioctl(fd, SIOCIFCREATE2, &mut request as *mut _ as *mut c_void) }
        } else {
            result
        }
    } else {
        result
    };
    unsafe { libc::close(fd) };
    if result < 0 {
        bail!("create interface: {}", io::Error::last_os_error());
    }
    read_interface_name(&request.ifr_name)
}

fn interface_module(kind: &str) -> Option<&'static str> {
    match kind {
        "bridge" => Some("if_bridge"),
        "epair" => Some("if_epair"),
        "tap" => Some("if_tap"),
        _ => None,
    }
}

pub fn exists(name: &str) -> Result<bool> {
    let mut interfaces = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut interfaces) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut current = interfaces;
    let mut found = false;
    while !current.is_null() {
        let interface = unsafe { (*current).ifa_name };
        if !interface.is_null()
            && unsafe { CStr::from_ptr(interface) }.to_bytes() == name.as_bytes()
        {
            found = true;
            break;
        }
        current = unsafe { (*current).ifa_next };
    }
    unsafe { libc::freeifaddrs(interfaces) };
    Ok(found)
}

pub fn destroy(name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    let result = ioctl(
        fd,
        SIOCIFDESTROY,
        &mut request as *mut _ as *mut c_void,
        "destroy interface",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn set_up(name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    let result = (|| {
        ioctl(
            fd,
            SIOCGIFFLAGS,
            &mut request as *mut _ as *mut c_void,
            "get interface flags",
        )?;
        unsafe {
            request.ifr_ifru.ifru_flags[0] |= libc::IFF_UP as i16;
        }
        ioctl(
            fd,
            SIOCSIFFLAGS,
            &mut request as *mut _ as *mut c_void,
            "set interface flags",
        )
    })();
    unsafe { libc::close(fd) };
    result
}

pub fn group_add(name: &str, group: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<Ifgroupreq>() };
    interface_name(&mut request.name, name)?;
    interface_name(unsafe { &mut request.data.group }, group)?;
    let result = ioctl(
        fd,
        SIOCAIFGROUP,
        &mut request as *mut _ as *mut c_void,
        "add interface group",
    );
    unsafe { libc::close(fd) };
    result
}

fn bridge_member(bridge: &str, member: &str, request: c_ulong) -> Result<()> {
    let fd = control_socket()?;
    let mut member_request = unsafe { std::mem::zeroed::<Ifbreq>() };
    interface_name(&mut member_request.member, member)?;
    let mut driver_request = unsafe { std::mem::zeroed::<libc::ifdrv>() };
    interface_name(&mut driver_request.ifd_name, bridge)?;
    driver_request.ifd_cmd = request;
    driver_request.ifd_len = size_of::<Ifbreq>();
    driver_request.ifd_data = &mut member_request as *mut _ as *mut c_void;
    let result = ioctl(
        fd,
        SIOCSDRVSPEC,
        &mut driver_request as *mut _ as *mut c_void,
        "change bridge membership",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn bridge_add(bridge: &str, member: &str) -> Result<()> {
    bridge_member(bridge, member, BRDGADD)
}

pub fn bridge_delete(bridge: &str, member: &str) -> Result<()> {
    bridge_member(bridge, member, BRDGDEL)
}

pub fn move_to_vnet(name: &str, jid: i32) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    request.ifr_ifru.ifru_jid = jid;
    let result = ioctl(
        fd,
        SIOCSIFVNET,
        &mut request as *mut _ as *mut c_void,
        "move interface to jail",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn rename(name: &str, new_name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    let mut replacement = [0_i8; IFNAMSIZ];
    interface_name(&mut request.ifr_name, name)?;
    interface_name(&mut replacement, new_name)?;
    request.ifr_ifru.ifru_data = replacement.as_mut_ptr().cast();
    let result = ioctl(
        fd,
        SIOCSIFNAME,
        &mut request as *mut _ as *mut c_void,
        "rename interface",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn ensure_bridge(bridge: &str, ipv4: Option<&str>, ipv6: Option<&str>) -> Result<()> {
    crate::util::cmd::message(&format!("Starting bridge {bridge}"));
    let bridge_exists = exists(bridge)?;
    if !bridge_exists {
        let created = create("bridge")?;
        if created != bridge {
            rename(&created, bridge)?;
        }
    }
    set_up(bridge)?;
    if let Some(ipv4) = ipv4 {
        set_ipv4(bridge, ipv4)?;
    }
    if let Some(ipv6) = ipv6 {
        set_ipv6(bridge, ipv6)?;
    }
    Ok(())
}

fn set_ipv4(interface: &str, address: &str) -> Result<()> {
    let address = address.parse::<std::net::Ipv4Addr>()?;
    let mut request = unsafe { std::mem::zeroed::<Ifaliasreq>() };
    interface_name(&mut request.name, interface)?;
    request.address = sockaddr_in4(libc::AF_INET as u8, address.octets());
    request.broadcast = sockaddr_in4(libc::AF_INET as u8, [255, 255, 255, 255]);
    request.mask = sockaddr_in4(libc::AF_INET as u8, [255, 255, 255, 0]);
    let fd = control_socket_family(libc::AF_INET)?;
    let result = ioctl(
        fd,
        SIOCAIFADDR,
        &mut request as *mut _ as *mut c_void,
        "set IPv4 interface address",
    );
    unsafe { libc::close(fd) };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("Address already in use") => Ok(()),
        Err(error) => Err(error),
    }
}

fn set_ipv6(interface: &str, address: &str) -> Result<()> {
    let address = address.parse::<std::net::Ipv6Addr>()?;
    let mut request = unsafe { std::mem::zeroed::<In6Aliasreq>() };
    interface_name(&mut request.name, interface)?;
    request.address = sockaddr_in6(address.octets());
    request.prefix_mask = sockaddr_in6([
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0, 0, 0, 0,
    ]);
    request.lifetime.valid = u32::MAX;
    request.lifetime.preferred_lifetime = u32::MAX;
    let fd = control_socket_family(libc::AF_INET6)?;
    let result = ioctl(
        fd,
        SIOCAIFADDR_IN6,
        &mut request as *mut _ as *mut c_void,
        "set IPv6 interface address",
    );
    unsafe { libc::close(fd) };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.to_string().contains("Address already in use") => Ok(()),
        Err(error) => Err(error),
    }
}

fn sockaddr_in4(family: u8, address: [u8; 4]) -> BsdSockaddr {
    let mut data = [0_i8; 14];
    data[2..6].copy_from_slice(&address.map(|byte| byte as i8));
    BsdSockaddr {
        len: 16,
        family,
        data,
    }
}

fn sockaddr_in6(address: [u8; 16]) -> BsdSockaddrIn6 {
    BsdSockaddrIn6 {
        len: 28,
        family: libc::AF_INET6 as u8,
        port: 0,
        flowinfo: 0,
        address,
        scope_id: 0,
    }
}

pub fn rename_in_jail(jid: i32, name: &str, new_name: &str) -> Result<()> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        if unsafe { libc::jail_attach(jid) } < 0 {
            unsafe { libc::_exit(libc::EIO) };
        }
        match rename(name, new_name) {
            Ok(()) => unsafe { libc::_exit(0) },
            Err(_) => unsafe { libc::_exit(libc::EIO) },
        }
    }
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        bail!("wait for interface rename: {}", io::Error::last_os_error());
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        bail!("rename interface in jail failed")
    }
}
