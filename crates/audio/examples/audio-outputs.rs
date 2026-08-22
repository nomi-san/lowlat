fn main() {
    match lowlat_audio::outputs(None) {
        Ok(found) => {
            println!("{} output(s):", found.len());
            for output in found {
                println!("  id={}\n    name={}", output.id, output.name);
            }
        }
        Err(error) => println!("failed: {error}"),
    }
}
