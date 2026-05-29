#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::{fs, path::Path};

#[cfg(test)]
pub(crate) static INTEGRATION_LOCK: Mutex<()> = Mutex::new(());
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn cleanup_fzy_io_files() {
    for path in [
        "/tmp/megaserver.fzy.control.input.json",
        "/tmp/megaserver.fzy.control.output.json",
        "/tmp/megaserver.fzy.host.input.json",
        "/tmp/megaserver.fzy.host.output.json",
        "/var/tmp/megaserver.fzy.control.input.json",
        "/var/tmp/megaserver.fzy.control.output.json",
        "/var/tmp/megaserver.fzy.host.input.json",
        "/var/tmp/megaserver.fzy.host.output.json",
    ] {
        let target = Path::new(path);
        if target.exists() {
            let _ = fs::remove_file(target);
        }
    }
}
