#![feature(async_closure)]

use anyhow::{Result, Context};
use clap::{Arg, App, ArgMatches};
use daemonize::{Daemonize, DaemonizeError};
use futures::{StreamExt, Stream, Sink, SinkExt};
use futures::executor::{block_on, block_on_stream};
use futures::future::{FutureExt, poll_fn, Future};
use futures::task::Poll;
use reqwest::StatusCode;
use serde_derive::{Deserialize, Serialize};
use std::fs::File;
use std::collections::HashMap;
use std::thread;
use std::process::exit;
use std::borrow::BorrowMut;
use std::time::Duration;
use telegram_bot as tg;
use telegram_bot::prelude::*;
use tokio;
use tokio::sync::mpsc;
use tokio::runtime::Runtime;
use tokio::sync;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prof1tConfigNotifierChannel {
    name: String,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prof1tConfigNotifier {
    #[serde(rename = "type")]
    notifier_type: String,
    token: String,
    channels: Vec<Prof1tConfigNotifierChannel>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Prof1tConfigTestType {
    #[serde(rename = "alive")]
    Alive
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prof1tConfigTest {
    #[serde(rename = "type")]
    test_type: Prof1tConfigTestType,
    name: String,
    retries: u64,
    recovery: u64,
    interval: u64,
    url: String,
    notify: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Prof1tConfig {
    notifiers: Vec<Prof1tConfigNotifier>,
    tests: Vec<Prof1tConfigTest>,
}

enum Prof1tNotifyType {
    Telegram(telegram_bot::Api),
}

type Prof1tChannel = (usize, String);
type Prof1tChannelMap = HashMap<String, Prof1tChannel>;
type Prof1tJobMessage = (usize, String);

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

struct Prof1t {
    config: Prof1tConfig,
    verbose: bool,

    notifiers: Vec<Prof1tNotifyType>,
    channels: Prof1tChannelMap,
}

impl Prof1t {
    fn new(
        mut config: Prof1tConfig,
        verbose: bool,
    ) -> Result<Prof1t> {
        let mut notifiers = Vec::new();

        let mut channels: Prof1tChannelMap = HashMap::new();

        for notifier in config.notifiers.iter() {
            match &*notifier.notifier_type {
                "telegram" => {
                    if notifier.channels.len() == 0 {
                        // there's no point to setup this listener
                        // if there's no channel associated with it
                        continue;
                    }

                    let notifier_id = notifiers.len();

                    notifiers.push(
                        Prof1tNotifyType::Telegram(
                            telegram_bot::Api::new(
                                notifier.token.clone(),
                            ),
                        ),
                    );

                    if verbose {
                        println!(
                            "Configured client (type: {}, token: {}, iid: {})..",
                            "telegram",
                            &notifier.token,
                            notifier_id,
                        );
                    }

                    for channel in notifier.channels.iter() {
                        if channel.id.clone().parse::<i64>().is_err() {
                            println!(
                                "Error: channel {:?} must have valid i64 id. (cid: {})",
                                channel.name.clone(),
                                channel.id.clone(),
                            );

                            exit(1);
                        }

                        channels.insert(
                            channel.name.clone(),
                            (notifier_id, channel.id.clone()),
                        );

                        if verbose {
                            println!(
                                "Registered channel (iid: {}, name: {}, id: {})..",
                                notifier_id,
                                channel.name.clone(),
                                channel.id.clone(),
                            );
                        }
                    }
                }
                unknown_type => {
                    println!("Error: type {:?} is unknown.", unknown_type);

                    exit(1);
                }
            }
        }

        for test in config.tests.iter() {
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
            Prof1t {
                config,
                notifiers,
                channels,

                verbose,
            },
        )
    }

    async fn try_send(
        &self,
        channel: &Prof1tChannel,
        msg: &str,
    ) -> Result<()> {
        let notifier =
            self.notifiers
                .get(channel.0)
                .unwrap();

        match notifier {
            Prof1tNotifyType::Telegram(api) => {
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
            }
        }

        Ok(())
    }

    async fn test_runner_alive(
        (test_id, test): (usize, Prof1tConfigTest),
        mut tx: tokio::sync::mpsc::Sender<Prof1tJobMessage>,
    ) -> Result<()> {
        let mut jsm = JobStateMachine::new(
            test.retries,
            test.recovery,
        );

        loop {
            let req =
                reqwest
                ::get(&test.url.clone())
                    .await;

            let assume_alive = match req {
                Ok(res) =>
                    res.status().is_success(),
                Err(_) => false,
            };

            if assume_alive {
                jsm.win();
            } else {
                jsm.loss();
            }

            //println!(
            //    "{} {:?} {:?} {}",
            //    jsm.state_changed(),
            //    jsm.state,
            //    jsm.last_state,
            //    jsm.last_state_lasted,
            //);

            if jsm.state_changed() {
                if jsm.state == JobSMStates::Failure {
                    tx.send((
                        test_id,
                        format!(
                            "<b>ALRT</b> Uptime checks failed on '{}'. (duration={}s, url: {})",
                            test.name,
                            jsm.last_state_lasted,
                            test.url,
                        )
                    )).await;
                }

                if jsm.state == JobSMStates::Recovery {
                    tx.send((
                        test_id,
                        format!(
                            "<b>RSLV</b> Uptime checks recovered on '{}'. (duration={}s, url: {})",
                            test.name,
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
        (test_id, test): (usize, Prof1tConfigTest),
        mut tx: tokio::sync::mpsc::Sender<Prof1tJobMessage>,
    ) -> Result<()> {
        match test.test_type {
            Prof1tConfigTestType::Alive =>
                Prof1t::test_runner_alive(
                    (test_id, test),
                    tx,
                ),
        }.await?;

        Ok(())
    }

    async fn handle_msg(
        &self,
        msg: Prof1tJobMessage,
    ) -> Result<()> {
        let testcase =
            self.config
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
            tokio::sync::mpsc::channel::<Prof1tJobMessage>(
                self.config.tests.len(),
            );

        for test in self.config.tests.iter().cloned().enumerate() {
            let mut test_tx = sender.clone();
            let (test_id, test) = test;

            tokio::spawn(async move {
                Prof1t::test_runner(
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

fn read_config(config_path: &str) -> Result<Prof1tConfig> {
    Ok(
        serde_yaml::from_str::<Prof1tConfig>(
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
                .help("Daemonize prof1t"))
            .arg(Arg::with_name("verbose")
                .short("v")
                .help("Print verbose logs"))
            .get_matches()
            .clone()
    };

    if matches.is_present("daemon") {
        try_daemonize()
            .context("Failed to daemonize process.")?;
    }

    let prof1t = Prof1t::new(
        read_config(
            matches.value_of("config").unwrap(),
        ).context("Failed to read config.")?,
        matches.is_present("verbose"),
    )?;

    prof1t.run().await?;

    Ok(())
}
