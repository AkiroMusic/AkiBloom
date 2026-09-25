use nih_plug::wrapper::standalone::nih_export_standalone_with_args;

use akibloom::AkiBloom;

fn main() {
    // Raise the default period size above nih-plug's 512-sample default (same
    // rationale as AkiFX: some WASAPI devices deliver packets larger than the
    // requested size and the cpal backend asserts). An explicit
    // `--period-size`/`-p` on the command line still takes precedence.
    let mut args: Vec<String> = std::env::args().collect();
    let user_set_period_size = args
        .iter()
        .any(|arg| arg == "--period-size" || arg == "-p");
    if !user_set_period_size {
        args.push("--period-size".to_owned());
        args.push("2048".to_owned());
    }

    let success = nih_export_standalone_with_args::<AkiBloom, _>(args);
    std::process::exit(if success { 0 } else { 1 });
}
