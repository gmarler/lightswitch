use clap::Parser;
use clap::ArgAction;

use tracing::Level;
use tracing_subscriber::FmtSubscriber;
use tracing_subscriber::fmt::format::FmtSpan;

use lightswitch::object::build_id;
use lightswitch::profiler::Collector;
use lightswitch::profiler::Profiler;
use lightswitch::unwind_info::{compact_printing_callback, UnwindInfoBuilder};
use std::error::Error;
use std::path::PathBuf;

use std::time::Duration;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    pids: Vec<i32>,
    #[arg(long)]
    show_unwind_info: Option<String>,
    #[arg(long)]
    show_info: Option<String>,
    #[arg(long)]
    continuous: bool,
    #[arg(long, action=ArgAction::SetFalse)]
    filter_logs: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();

    let subscriber = FmtSubscriber::builder()
    .with_max_level(if args.filter_logs {Level::TRACE} else {Level::INFO})
    .with_span_events(FmtSpan::ENTER | FmtSpan::CLOSE)
    .finish();

    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");


    if let Some(path) = args.show_unwind_info {
        UnwindInfoBuilder::with_callback(&path, compact_printing_callback)?.process()?;
        return Ok(());
    }

    if let Some(path) = args.show_info {
        println!("build id {:?}", build_id(&PathBuf::from(path.clone())));
        let unwind_info: Result<UnwindInfoBuilder<'_>, anyhow::Error> =
            UnwindInfoBuilder::with_callback(&path, |_| {});
        println!("unwind info {:?}", unwind_info.unwrap().process());

        return Ok(());
    }


    let mut duration = Duration::MAX;
    if !args.continuous {
        duration = Duration::from_secs(3);
    }

    let collector = Collector::new();

    let mut p: Profiler<'_> = Profiler::new(false);
    p.profile_pids(args.pids);

    p.run(duration, collector.clone());
    collector.lock().unwrap().finish();

    Ok(())
}
