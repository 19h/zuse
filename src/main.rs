#![feature(async_closure)]

use anyhow::{Result, Context};
use clap::{Arg, App};
use daemonize::Daemonize;
use futures::{StreamExt, SinkExt};
use serde_derive::{Deserialize, Serialize};
use std::fs::File;
use std::collections::HashMap;
use std::process::exit;
use std::time::Duration;
use telegram_bot::prelude::*;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifierChannel {
    name: String,

    // Telegram

    id: Option<String>,

    // AwsSns

    phone: Option<String>,
    target_arn: Option<String>,
    topic_arn: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifierAuth {
    // Telegram

    token: Option<String>,

    // AwsSns

    key: Option<String>,
    secret: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifier {
    #[serde(rename = "type")]
    notifier_type: String,
    auth: ZuseConfigNotifierAuth,
    channels: Vec<ZuseConfigNotifierChannel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum ZuseConfigTestType {
    #[serde(rename = "alive")]
    Alive
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigTest {
    #[serde(rename = "type")]
    test_type: ZuseConfigTestType,
    name: String,
    retries: u64,
    recovery: u64,
    interval: u64,
    url: String,
    notify: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigInternal {
    dump_prefix_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfig {
    notifiers: Vec<ZuseConfigNotifier>,
    config: Option<ZuseConfigInternal>,
    tests: Vec<ZuseConfigTest>,
}

#[derive(Clone)]
struct ZuseArgs {
    verbosity: u8,
    config: ZuseConfig,
}

impl ZuseArgs {
    #[inline(always)]
    fn debug(&self) -> bool { self.verbosity > 1 }

    #[inline(always)]
    fn verbose(&self) -> bool { self.verbosity > 0 }
}

enum ZuseNotifyType {
    Telegram(telegram_bot::Api),
    Sns(rusoto_sns::SnsClient),
}

type ZuseChannel = (usize, String);
type ZuseChannelMap = HashMap<String, ZuseChannel>;
type ZuseJobMessage = (usize, String);

#[derive(Debug, Clone, PartialEq)]
enum JobSMStates {
    Normative,
    Failure,
    Recovery,
}

struct JobStateMachine {
    n_failures: u64,
    n_recoveries: u64,

    state: JobSMStates,
    last_state: JobSMStates,

    last_state_lasted: u64,
    last_change: std::time::SystemTime,

    retries: u64,
    recovery: u64,
}

impl JobStateMachine {
    fn new(
        retries: u64,
        recovery: u64,
    ) -> JobStateMachine {
        JobStateMachine {
            n_failures: 0,
            n_recoveries: 0,

            state: JobSMStates::Normative,
            last_state: JobSMStates::Normative,

            last_state_lasted: 0u64,
            last_change: std::time::SystemTime::now(),

            retries,
            recovery,
        }
    }

    fn state_changed(&self) -> bool {
        !self.last_state.eq(&self.state)
    }

    fn state(&mut self, state: JobSMStates) {
        self.last_state = self.state.clone();
        self.state = state;

        if self.state_changed() {
            self.last_state_lasted = {
                match self.last_change.elapsed() {
                    Ok(dur) => dur.as_secs(),
                    Err(_) => 0u64,
                }
            };

            self.last_change = std::time::SystemTime::now();
        }
    }

    fn loss(&mut self) {
        self.n_failures += 1;
        self.n_recoveries = 0;

        match self.state {
            JobSMStates::Normative =>
                if self.n_failures >= self.retries {
                    self.state(JobSMStates::Failure);
                },
            JobSMStates::Failure => {
                self.last_state = JobSMStates::Failure;
            }
            JobSMStates::Recovery => {
                self.state(JobSMStates::Failure);
            }
        }
    }

    fn win(&mut self) {
        match self.state {
            JobSMStates::Normative => {
                self.n_failures = 0;
                self.last_state = JobSMStates::Normative;
            }
            JobSMStates::Failure => {
                self.n_recoveries += 1;

                if self.n_recoveries >= self.recovery {
                    self.n_failures = 0;
                    self.n_recoveries = 0;

                    self.state(JobSMStates::Recovery);
                }
            }
            JobSMStates::Recovery => {
                self.last_state = JobSMStates::Recovery;
            }
        }
    }

    fn normative(&mut self) {
        match self.state {
            JobSMStates::Normative => {}
            JobSMStates::Failure => {}
            JobSMStates::Recovery => {
                self.state(JobSMStates::Normative);
            }
        }
    }
}

struct Zuse {
    args: ZuseArgs,

    notifiers: Vec<ZuseNotifyType>,
    channels: ZuseChannelMap,
}

impl Zuse {
    fn new(
        mut args: ZuseArgs,
    ) -> Result<Zuse> {
        let mut notifiers = Vec::new();

        let mut channels: ZuseChannelMap = HashMap::new();

        for notifier in args.config.notifiers.iter() {
            if notifier.channels.is_empty() {
                // there's no point to setup this listener
                // if there's no channel associated with it
                continue;
            }

            let notifier_id = notifiers.len();

            match &*notifier.notifier_type {
                "telegram" => {
                    notifier.auth.token
                        .as_ref()
                        .expect(
                            &format!(
                                "Error: notifier {:?} ({}) must specify an id.",
                                &notifier_id,
                                &notifier.notifier_type,
                            ),
                        );

                    let token =
                        notifier.auth.token
                            .as_ref()
                            .unwrap()
                            .clone();

                    notifiers.push(
                        ZuseNotifyType::Telegram(
                            telegram_bot::Api::new(
                                token.clone(),
                            ),
                        ),
                    );

                    if args.verbose() {
                        println!(
                            "Configured client (type: {}, token: {}, iid: {})..",
                            "telegram",
                            &token,
                            notifier_id,
                        );
                    }

                    for channel in notifier.channels.iter() {
                        let chan_id =
                            channel.id
                                .as_ref()
                                .expect(
                                    &format!(
                                        "Error: channel {:?} must specify an id.",
                                        &channel.name,
                                    ),
                                );

                        chan_id.parse::<i64>()
                            .expect(
                                &format!(
                                    "Error: channel {:?} must have valid i64 id. (cid: {})",
                                    &channel.name,
                                    chan_id,
                                ),
                            );

                        channels.insert(
                            channel.name.clone(),
                            (notifier_id, chan_id.clone()),
                        );

                        if args.verbose() {
                            println!(
                                "Registered channel (iid: {}, name: {}, id: {})..",
                                notifier_id,
                                channel.name.clone(),
                                chan_id.clone(),
                            );
                        }
                    }
                }
                "sns" => {

                },
                unknown_type => {
                    println!("Error: type {:?} is unknown.", unknown_type);

                    exit(1);
                }
            }
        }

        for test in args.config.tests.iter() {
            for test_notify in test.notify.iter() {
                if !channels.contains_key(test_notify) {
                    println!(
                        "Error: '{}' (type: {:?}) references inexistent channel: {}",
                        test.name,
                        test.test_type,
                        test_notify,
                    );

                    exit(1);
                }
            }
        }

        Ok(
            Zuse {
                args,

                notifiers,
                channels,
            },
        )
    }

    async fn try_send(
        &self,
        channel: &ZuseChannel,
        msg: &str,
    ) -> Result<()> {
        let notifier =
            self.notifiers
                .get(channel.0)
                .unwrap();

        match notifier {
            ZuseNotifyType::Telegram(api) => {
                let mut msg =
                    telegram_bot::ChannelId::new(
                        channel.1.clone().parse::<i64>().unwrap()
                    )
                        .text(msg);

                msg
                    .parse_mode(telegram_bot::types::ParseMode::Html)
                    .disable_preview();

                api.send(
                    msg,
                ).await?;
            },
            ZuseNotifyType::Sns(client) => {
                unimplemented!();
            }
        }

        Ok(())
    }

    async fn test_runner_alive(
        args: &ZuseArgs,
        (test_id, test): (usize, ZuseConfigTest),
        mut tx: tokio::sync::mpsc::Sender<ZuseJobMessage>,
    ) -> Result<()> {
        let mut jsm = JobStateMachine::new(
            test.retries,
            test.recovery,
        );

        let dump_req = {
            if args.config.config.is_some()
                && args.config.config.as_ref().unwrap().dump_prefix_url.is_some() {
                (true, &args.config.config.as_ref().unwrap().dump_prefix_url)
            } else {
                (false, &None)
            }
        };

        loop {
            let req =
                reqwest
                ::get(&test.url.clone())
                    .await;

            let serialized_req =
                if dump_req.0 {
                    format!("{:#?}", req)
                } else {
                    "".to_string()
                };

            let assume_alive = match req {
                Ok(res) => {
                    res.status().is_success()
                }
                Err(_) => false,
            };

            if assume_alive {
                jsm.win();
            } else {
                jsm.loss();
            }

            if args.debug() {
                println!(
                    "alive: {} changed: {} now: {:?} was: {:?} lasted: {}",
                    assume_alive,
                    jsm.state_changed(),
                    jsm.state,
                    jsm.last_state,
                    jsm.last_state_lasted,
                );
            }

            if jsm.state_changed() {
                if jsm.state == JobSMStates::Failure {
                    tx.send((
                        test_id,
                        format!(
                            "<b>ALRT</b> Uptime checks failed on '{}'. ({}url: {})",
                            test.name,
                            {
                                if dump_req.0 {
                                    format!(
                                        "<a href='{}#{}'>view dump</a>, ",
                                        &dump_req.1.as_ref().unwrap(),
                                        base64::encode(&serialized_req),
                                    )
                                } else {
                                    "".to_string()
                                }
                            },
                            test.url,
                        )
                    )).await;
                }

                if jsm.state == JobSMStates::Recovery {
                    tx.send((
                        test_id,
                        format!(
                            "<b>RSLV</b> Uptime checks recovered on '{}'. (<a href='{}'>view dump</a>, duration={}s, url: {})",
                            test.name,
                            format!(
                                "https://r2.darknet.dev/b64d.html#{}",
                                base64::encode(&serialized_req),
                            ),
                            jsm.last_state_lasted,
                            test.url,
                        )
                    )).await;

                    jsm.normative();
                }
            }

            tokio::time::delay_for(
                Duration::from_secs(
                    test.interval,
                ),
            ).await;
        }

        Ok(())
    }

    async fn test_runner(
        args: ZuseArgs,
        (test_id, test): (usize, ZuseConfigTest),
        mut tx: tokio::sync::mpsc::Sender<ZuseJobMessage>,
    ) -> Result<()> {
        match test.test_type {
            ZuseConfigTestType::Alive =>
                Zuse::test_runner_alive(
                    &args,
                    (test_id, test),
                    tx,
                ),
        }.await?;

        Ok(())
    }

    async fn handle_msg(
        &self,
        msg: ZuseJobMessage,
    ) -> Result<()> {
        let testcase =
            self.args
                .config
                .tests
                .get(msg.0)
                .unwrap();

        for notify_channel in testcase.notify.iter() {
            if let Some(channel)
            = self.channels.get(notify_channel) {
                let send_op = self.try_send(
                    &channel,
                    &msg.1,
                ).await;

                if self.args.debug() {
                    println!(
                        "notify dispatch. chan: {} msg: {}",
                        &channel.1,
                        &msg.1,
                    );
                }

                send_op
                    .map_err(|err|
                        println!(
                            "WARNING: SENDING MESSAGE VIA CHANNEL {} FAILED: {:?}",
                            &channel.1,
                            &err,
                        )
                    );
            }
        }

        Ok(())
    }

    async fn run(self) -> Result<()> {
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<ZuseJobMessage>(
                self.args.config.tests.len(),
            );

        for test in self.args.config.tests.iter().cloned().enumerate() {
            let mut test_tx = sender.clone();
            let (test_id, test) = test;

            let args = self.args.clone();

            tokio::spawn(async move {
                Zuse::test_runner(
                    args,
                    (test_id, test),
                    test_tx,
                ).await;
            });
        }

        loop {
            match receiver.recv().await {
                Some(msg) => self.handle_msg(msg).await?,
                None => {}
            }
        }
    }
}

fn try_daemonize() -> Result<()> {
    let stdout = File::create("/dev/null").unwrap();
    let stderr = File::create("/dev/null").unwrap();

    let daemonize = Daemonize::new()
        .user("nobody")
        .group("daemon")
        .stdout(stdout)
        .stderr(stderr)
        .exit_action(|| println!("Executed before master process exits"))
        .privileged_action(|| "Failed to drop privileges..");

    daemonize.start()?;

    Ok(())
}

fn read_config(config_path: &str) -> Result<ZuseConfig> {
    Ok(
        serde_yaml::from_str::<ZuseConfig>(
            &String::from_utf8_lossy(
                &std::fs::read(&config_path)?,
            ),
        )?,
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let matches = {
        App::new("prof1t")
            .version("1.0.0")
            .author("Kenan Sulayman <kenan@sig.dev>")
            .about("Testbot with notify function.")
            .arg(Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("CONFIG")
                .help("Config file")
                .takes_value(true)
                .default_value("tests.yml"))
            .arg(Arg::with_name("daemon")
                .short("d")
                .long("daemon")
                .help("Daemonize prof1t"))
            .arg(Arg::with_name("verbose")
                .short("v")
                .long("verbose")
                .help("Print verbose logs"))
            .arg(Arg::with_name("debug")
                .long("debug")
                .help("Print debug logs (implies verbose)"))
            .get_matches()
            .clone()
    };

    if matches.is_present("daemon") {
        try_daemonize()
            .context("Failed to daemonize process.")?;
    }

    let config = read_config(
        matches.value_of("config").unwrap(),
    ).context("Failed to read config.")?;

    let args = ZuseArgs {
        verbosity: {
            if matches.is_present("debug") {
                2
            } else if matches.is_present("verbose") {
                1
            } else {
                0
            }
        },
        config,
    };

    let prof1t = Zuse::new(
        args,
    )?;

    prof1t.run().await?;

    Ok(())
}
