//! `lowlat-vdd`: the host's virtual displays from a shell, through the
//! driver's controller (docs/11-drivers.md section 1.9) -- the commands
//! `lowlatd display` takes, for a machine that runs no host.

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(lowlat_vdd_ctl::cli::run("lowlat-vdd", &args))
}

#[cfg(not(windows))]
fn main() {
    eprintln!("lowlat-vdd: the virtual displays are the Windows driver's; there is none here");
    std::process::exit(1)
}
