//! Throwaway: make a library the app's way under a given folder.
#![allow(clippy::pedantic, clippy::print_stdout, clippy::unwrap_used, clippy::expect_used)]
fn main() {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap());
    let plan = rbl_db::new_library::plan_at(&root.join("library")).unwrap().unwrap();
    let made = rbl_db::new_library::create(&plan).unwrap();
    println!("{}", made.master_db.display());
}
