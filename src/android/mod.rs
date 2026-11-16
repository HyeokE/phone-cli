//! Android phones over adb. No app is installed: reading and input go
//! through a small helper run with `app_process` from /data/local/tmp
//! ([`helper`]), or through the shell tools Android ships when it is off or
//! cannot run.

pub mod adb;
pub mod devices;
pub mod doctor;
pub mod helper;
pub mod keys;
pub mod session;
pub mod uia;
