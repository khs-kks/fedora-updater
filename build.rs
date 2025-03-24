use chrono::{DateTime, Utc};
use std::time::SystemTime;

fn main() {
    // Get the current date in a formatted string
    let now = SystemTime::now();
    let datetime: DateTime<Utc> = now.into();

    // Format date as Day Month Year (e.g., "15 Mar 2024")
    let build_date = datetime.format("%d %b %Y").to_string();

    // Tell Cargo to re-run this script if build.rs changes
    println!("cargo:rerun-if-changed=build.rs");

    // Pass the build date to the Rust code as an environment variable
    println!("cargo:rustc-env=CARGO_BUILD_DATE={}", build_date);
}
