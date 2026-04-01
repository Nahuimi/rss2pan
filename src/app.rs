use std::path::PathBuf;

use clap::builder::PossibleValuesParser;
use clap::{arg, crate_version, value_parser, Arg, ArgAction, Command};

const QRCODE_APPS: &[&str] = &[
    "web",
    "android",
    "115android",
    "ios",
    "115ipad",
    "tv",
    "alipaymini",
    "wechatmini",
    "qandroid",
    "115ios",
];

pub fn build_app() -> Command {
    let app = Command::new("rss2pan")
        .version(crate_version!())
        .about("rss to pan")
        .arg(arg!(-r --rss [rss] "rss.json path").value_parser(value_parser!(PathBuf)))
        .arg(arg!(-u --url [url] "rss url"))
        .arg(arg!(-m --concurrent "concurrent request").action(ArgAction::SetTrue))
        .arg(arg!(--cookies [cookies] "115 cookies"))
        .arg(arg!(-q --qrcode "login 115 by qrcode").action(ArgAction::SetTrue))
        .arg(
            Arg::new("qrcode-app")
                .long("qrcode-app")
                .help("qrcode login app")
                .default_value("tv")
                .requires("qrcode")
                .value_parser(PossibleValuesParser::new(QRCODE_APPS)),
        )
        .arg(arg!(--"no-cache" "skip checking cache in db.sqlite").action(ArgAction::SetTrue))
        .arg(
            arg!(--"chunk-delay" [chunk_delay] "chunk delay in seconds")
                .value_parser(value_parser!(u64)),
        )
        .arg(
            arg!(--"chunk-size" [chunk_size] "chunk size for offline tasks")
                .value_parser(value_parser!(usize)),
        )
        .arg(
            arg!(--"clear-task-type" [clear_task_type] "clear offline task type: 1-6")
                .value_parser(value_parser!(u8).range(1..=6)),
        )
        .subcommand(
            Command::new("magnet")
                .about("magnet to pan")
                .arg(arg!(-l --link [link] "magnet link").conflicts_with("txt"))
                .arg(
                    Arg::new("txt")
                        .long("txt")
                        .visible_alias("text")
                        .help("magnet txt file")
                        .value_parser(value_parser!(PathBuf))
                        .conflicts_with("link"),
                )
                .arg(arg!(--cid [cid] "folder id in wangpan"))
                .arg(arg!(--savepath [savepath] "save path under cid/root")),
        )
        .subcommand(
            Command::new("server").about("start server").arg(
                arg!(-p --port [port] "server port")
                    .value_parser(value_parser!(u16))
                    .default_value("8115"),
            ),
        );

    app
}

#[test]
fn t_cmd() {
    let cmd = build_app();
    let matches = cmd.clone().try_get_matches_from(["rss2pan", "-m"]).unwrap();
    assert_eq!(matches.get_one::<bool>("concurrent").copied(), Some(true));
    let matches = cmd.clone().try_get_matches_from(["rss2pan"]).unwrap();
    assert_eq!(matches.get_one::<bool>("concurrent").copied(), Some(false));
}

#[test]
fn t_subcomd() {
    let cmd = build_app();
    let matches = cmd
        .clone()
        .try_get_matches_from([
            "rss2pan",
            "magnet",
            "--cid",
            "21345",
            "--link",
            "magnet:?xt=urn:btih:12345",
        ])
        .unwrap();
    match matches.subcommand() {
        Some(("magnet", matches)) => {
            assert_eq!(
                matches.get_one::<String>("cid").cloned(),
                Some("21345".to_string())
            );
            assert_eq!(
                matches.get_one::<String>("link").cloned(),
                Some("magnet:?xt=urn:btih:12345".to_string())
            );
        }
        _ => panic!("subcommand not found"),
    }
    let matches = cmd
        .clone()
        .try_get_matches_from(["rss2pan", "magnet", "--txt", "magnet.txt"])
        .unwrap();
    match matches.subcommand() {
        Some(("magnet", matches)) => {
            assert_eq!(
                matches.get_one::<PathBuf>("txt").cloned(),
                Some(PathBuf::from("magnet.txt"))
            );
        }
        _ => panic!("subcommand not found"),
    }
}

#[test]
fn t_new_flags() {
    let cmd = build_app();
    let matches = cmd
        .try_get_matches_from([
            "rss2pan",
            "--cookies",
            "UID=1;CID=2;SEID=3",
            "--qrcode",
            "--qrcode-app",
            "ios",
            "--no-cache",
            "--chunk-delay",
            "3",
            "--chunk-size",
            "50",
            "--clear-task-type",
            "2",
        ])
        .unwrap();
    assert_eq!(
        matches.get_one::<String>("cookies").map(|s| s.as_str()),
        Some("UID=1;CID=2;SEID=3")
    );
    assert_eq!(matches.get_one::<bool>("qrcode").copied(), Some(true));
    assert_eq!(
        matches.get_one::<String>("qrcode-app").map(|s| s.as_str()),
        Some("ios")
    );
    assert_eq!(matches.get_one::<bool>("no-cache").copied(), Some(true));
    assert_eq!(matches.get_one::<u64>("chunk-delay").copied(), Some(3));
    assert_eq!(matches.get_one::<usize>("chunk-size").copied(), Some(50));
    assert_eq!(matches.get_one::<u8>("clear-task-type").copied(), Some(2));
}
