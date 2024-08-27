use std::time::Duration;

use indicatif::ProgressBar;
use oma_contents::searcher::{ripgrep_search, Mode};

fn main() {
    let pb = ProgressBar::new_spinner();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_message("Searching ...");

    ripgrep_search("/var/lib/apt/lists", Mode::Files, "apt", |(pkg, file)| {
        println!("{pkg}: {file}")
    })
    .unwrap();
}
