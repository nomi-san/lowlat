//! Print this session's display layout, and every change to it.

fn main() {
    let Some((mut watch, first)) = lowlat_capture::desktop::Watch::open() else {
        eprintln!("no session to watch");
        std::process::exit(1);
    };
    say("first", &first);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(25);
    while std::time::Instant::now() < until {
        if let Some(outputs) = watch.changed(std::time::Duration::from_millis(500)) {
            say("changed", &outputs);
        }
    }
}

fn say(what: &str, outputs: &[lowlat_capture::desktop::Output]) {
    let mut line = String::new();
    for output in outputs {
        line.push_str(&format!(
            " {}={:?}x{:?}@{:?},{:?}",
            output.name.as_deref().unwrap_or("?"),
            output.width,
            output.height,
            output.x,
            output.y
        ));
    }
    println!("{what}:{line}");
}
