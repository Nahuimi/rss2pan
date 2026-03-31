mod app;
mod db;
mod downloader;
mod m115;
mod pan115;
mod request;
mod rss_config;
mod rss_site;
mod runner;
mod server;
mod utils;

use std::path::PathBuf;

use app::build_app;
use db::RssService;
use pan115::Pan115Client;
use request::Ajax;
use runner::{RunOptions, TaskRunner};
use utils::get_magnet_list_by_txt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logger();

    let matches = build_app().get_matches();
    let ajax = Ajax::from_matches(&matches);
    let pan115 = Pan115Client::new(ajax.clone());
    let rss_path = matches.get_one::<PathBuf>("rss").cloned();
    let runner = TaskRunner::new(
        pan115.clone(),
        ajax,
        rss_path,
        RunOptions::from_matches(&matches),
    );

    if let Some(clear_task_type) = matches.get_one::<u8>("clear-task-type").copied() {
        pan115.ensure_logged_in().await?;
        pan115.clear_offline_tasks(clear_task_type - 1).await?;
        return Ok(());
    }

    if let Some(("server", matches)) = matches.subcommand() {
        pan115.ensure_logged_in().await?;
        let port = matches.get_one::<u16>("port").copied().unwrap_or(8115);
        server::serve(pan115, runner.options(), port).await?;
        return Ok(());
    }

    if let Some(("magnet", matches)) = matches.subcommand() {
        let link = matches.get_one::<String>("link").cloned();
        let txt = matches.get_one::<PathBuf>("txt").cloned();
        let cid = matches.get_one::<String>("cid").cloned();
        let savepath = matches.get_one::<String>("savepath").cloned();

        let mut magnets = Vec::new();
        if let Some(path) = txt {
            magnets = get_magnet_list_by_txt(&path)?;
        } else if let Some(link) = link {
            magnets.push(link);
        } else {
            eprintln!("magnet link or txt file is required");
            std::process::exit(1);
        }

        if let Err(err) = runner.execute_links(&magnets, cid, savepath).await {
            print_error(&err);
            std::process::exit(1);
        }
        return Ok(());
    }

    let service = RssService::new()?;
    if let Some(url) = matches.get_one::<String>("url") {
        if let Err(err) = runner.execute_url(&service, url).await {
            print_error(&err);
            std::process::exit(1);
        }
        return Ok(());
    }

    let result = if matches.get_one::<bool>("concurrent").copied() == Some(true) {
        runner.execute_all_concurrent(&service).await
    } else {
        runner.execute_all(&service).await
    };
    if let Err(err) = result {
        print_error(&err);
        std::process::exit(1);
    }

    Ok(())
}

fn print_error(err: &anyhow::Error) {
    eprintln!("{err}");
    for cause in err.chain().skip(1) {
        eprintln!("caused by: {cause}");
    }
}

fn init_logger() {
    use std::io::Write;

    let env = env_logger::Env::default().default_filter_or("info");
    let mut builder = env_logger::Builder::from_env(env);
    builder.format(|buf, record| {
        let level_style = buf.default_level_style(record.level());
        writeln!(
            buf,
            "[{level_style}{}{level_style:#}] {}",
            record.level(),
            record.args()
        )
    });
    builder.init();
}
