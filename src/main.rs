#![feature(async_closure)]

use anyhow::{Result, Context};
use clap::{Arg, App};
use daemonize::Daemonize;
use futures::{StreamExt, SinkExt};
use serde_derive::{Deserialize, Serialize};
use std::fs::File;
use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::time::Duration;
use telegram_bot::prelude::*;
use rusoto_core::{Region, HttpClient, credential};
use rusoto_core::credential::ProvideAwsCredentials;
use rusoto_sns::{Sns, CheckIfPhoneNumberIsOptedOutInput, MessageAttributeValue};
use rusoto_sts::{Sts, GetCallerIdentityRequest, GetCallerIdentityResponse};

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
    region: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifierTemplates {
    alive_alert_subject: Option<String>,
    alive_alert_plain: Option<String>,
    alive_alert_html: Option<String>,
    alive_resolve_subject: Option<String>,
    alive_resolve_plain: Option<String>,
    alive_resolve_html: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum ZuseConfigNotifierType {
    #[serde(rename = "telegram")]
    Telegram,
    #[serde(rename = "sns")]
    Sns,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifier {
    #[serde(rename = "type")]
    notifier_type: ZuseConfigNotifierType,
    sender_id: Option<String>,
    auth: ZuseConfigNotifierAuth,
    templates: Option<ZuseConfigNotifierTemplates>,
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
    timeout: Option<u64>,
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

#[derive(Debug, Clone)]
enum ZuseChannelType {
    Telegram(String),
    Sns(Option<String>, Option<String>, Option<String>),
}

type ZuseChannel = (usize, ZuseChannelType);
type ZuseChannelMap = HashMap<String, ZuseChannel>;

const DEFAULT_SENDER_ID: &'static str = "NOTICE";

const DEFAULT_MSG_TMPL_ALIVE_ALRT_SUBJECT: &'static str = "ALRT {{test_name}}";
const DEFAULT_MSG_TMPL_ALIVE_ALRT_PLAIN: &'static str = "ALRT Uptime checks failed on '{{test_name}}'. (url: {{test_url}})";
const DEFAULT_MSG_TMPL_ALIVE_ALRT_HTML: &'static str = "<b>ALRT</b> Uptime checks failed on '{{test_name}}'. (url: {{test_url}})";

const DEFAULT_MSG_TMPL_ALIVE_RSLV_SUBJECT: &'static str = "RSVL {{test_name}}";
const DEFAULT_MSG_TMPL_ALIVE_RSLV_PLAIN: &'static str = "RSLV Uptime checks recovered on '{{test_name}}'. (duration={{time_state_lasted}}s, url: {{test_url}})";
const DEFAULT_MSG_TMPL_ALIVE_RSLV_HTML: &'static str = "<b>RSLV</b> Uptime checks recovered on '{{test_name}}'. (duration={{time_state_lasted}}s, url: {{test_url}})";

#[derive(Debug, Clone, Serialize)]
struct ZuseJobMessage {
    test_id: usize,
    test_name: String,
    test_url: String,
    dump_html: String,
    dump_url: String,
    dump_used: bool,
    time_state_lasted: u64,
    state: JobSMStates,
}

impl ZuseJobMessage {
    fn with_state(
        self,
        state: JobSMStates,
    ) -> Self {
        Self {
            state,
            ..self
        }
    }

    fn resolve_custom_templates(
        &self,
        notifier: &ZuseConfigNotifier,
    ) -> (String, String, String) {
        let tmpl_cstm =
            notifier
                .templates
                .as_ref()
                .map_or(
                    (None, None, None, None, None, None,),
                    |t|
                        (
                            t.alive_alert_subject.clone(),
                            t.alive_alert_html.clone(),
                            t.alive_alert_plain.clone(),
                            t.alive_resolve_subject.clone(),
                            t.alive_resolve_html.clone(),
                            t.alive_resolve_plain.clone(),
                        )
                );

        match &self.state {
            JobSMStates::Failure => {
                (
                    tmpl_cstm.0.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_ALRT_SUBJECT.to_string()),
                    tmpl_cstm.1.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_ALRT_HTML.to_string()),
                    tmpl_cstm.2.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_ALRT_PLAIN.to_string())
                )
            },
            JobSMStates::Recovery => {
                (
                    tmpl_cstm.3.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_ALRT_SUBJECT.to_string()),
                    tmpl_cstm.4.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_RSLV_HTML.to_string()),
                    tmpl_cstm.5.unwrap_or(DEFAULT_MSG_TMPL_ALIVE_RSLV_PLAIN.to_string())
                )
            },
            JobSMStates::Normative => unreachable!(),
        }
    }

    fn build(
        &self,
        args: &ZuseArgs,
        notifier_id: &usize,
    ) -> (String, String, String) {
        let notifier =
            args
                .config
                .notifiers
                .get(*notifier_id)
                .unwrap();

        let (tmpl_subject, tmpl_html, tmpl_plain) =
            self.resolve_custom_templates(
                &notifier,
            );

        // TODO: if user wants to selectively make
        // a platform plain or html, impl it here
        let rdr_tmpls = match notifier.notifier_type {
            ZuseConfigNotifierType::Telegram => {
                handlebars::Handlebars::new()
                    .render_template(
                        &*tmpl_html,
                        &self,
                    )
                    .unwrap()
            },
            ZuseConfigNotifierType::Sns => {
                handlebars::Handlebars::new()
                    .render_template(
                        &*tmpl_plain,
                        &self,
                    )
                    .unwrap()
            },
        };

        let rdr_subject = handlebars::Handlebars::new()
            .render_template(
                &*tmpl_subject,
                &self,
            )
            .unwrap();

        let sender_id =
            notifier
                .sender_id
                .clone()
                .unwrap_or(DEFAULT_SENDER_ID.to_string());

        (sender_id, rdr_subject, rdr_tmpls)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
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
    async fn new(
        mut args: ZuseArgs,
    ) -> Result<Zuse> {
        let mut notifiers = Vec::new();

        let mut channels: ZuseChannelMap = HashMap::new();

        for (notifier_id, notifier) in args.config.notifiers.iter().enumerate() {
            if notifier.channels.is_empty() {
                // there's no point to setup this listener
                // if there's no channel associated with it
                continue;
            }

            match notifier.notifier_type {
                ZuseConfigNotifierType::Telegram => {
                    if let None = notifier.auth.token.as_ref() {
                        println!(
                            "Error: notifier {:?} ({:?}) must specify a token.",
                            &notifier_id,
                            &notifier.notifier_type,
                        );

                        exit(1);
                    }

                    let token =
                        notifier.auth.token
                            .as_ref()
                            .unwrap()
                            .clone();

                    let mut api = telegram_bot::Api::new(
                        token.clone(),
                    );

                    if args.verbose() {
                        println!(
                            "Configured notifier {} (type: {:?}, token: {})..",
                            notifier_id,
                            &notifier.notifier_type,
                            &token,
                        );
                    }

                    if api.send(
                        telegram_bot::GetMe,
                    ).await.is_err() {
                        println!(
                            "Error: notifier {} ({:?}) has invalid telegram token. (token: {})",
                            notifier_id,
                            &notifier.notifier_type,
                            &token,
                        );

                        exit(1);
                    }

                    for channel in notifier.channels.iter() {
                        let chan_id =
                            match channel.id.as_ref() {
                                Some(cid) => cid,
                                None => {
                                    println!(
                                        "Error: channel {:?} must specify an id.",
                                        &channel.name,
                                    );

                                    exit(1);
                                }
                            };

                        if api.send(
                            telegram_bot::ChannelId::new(
                                chan_id.clone().parse::<i64>().unwrap()
                            ).get_chat()
                        ).await.is_err() {
                            println!(
                                "Error: channel {:?} could not be validated. (cid: {})",
                                &channel.name,
                                chan_id,
                            );

                            exit(1);
                        }

                        if let Err(_) = chan_id.parse::<i64>() {
                            println!(
                                "Error: channel {:?} must have valid i64 id. (cid: {})",
                                &channel.name,
                                chan_id,
                            );

                            exit(1);
                        }

                        channels.insert(
                            channel.name.clone(),
                            (
                                notifier_id,
                                ZuseChannelType::Telegram(
                                    chan_id.clone(),
                                ),
                            ),
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

                    notifiers.push(
                        ZuseNotifyType::Telegram(
                            api,
                        ),
                    );
                }
                ZuseConfigNotifierType::Sns => {
                    let has_key_and_secret =
                        notifier.auth.key.is_some()
                            && notifier.auth.secret.is_some();

                    if let None = notifier.auth.region {
                        println!(
                            "Error: notifier {:?} ({:?}) must specify region.",
                            &notifier_id,
                            &notifier.notifier_type,
                        );

                        exit(1);
                    }

                    let region =
                        (*notifier.auth.region.as_ref().unwrap()).parse::<Region>()?;

                    let (mut sts_client, mut sns_client) = {
                        if has_key_and_secret {
                            let cred = credential::StaticProvider::new(
                                notifier.auth.key.as_ref().unwrap().clone(),
                                notifier.auth.secret.as_ref().unwrap().clone(),
                                None,
                                None,
                            );

                            (
                                rusoto_sts::StsClient::new_with(
                                    HttpClient::new()?,
                                    cred.clone(),
                                    region.clone(),
                                ),
                                rusoto_sns::SnsClient::new_with(
                                    HttpClient::new()?,
                                    cred,
                                    region,
                                )
                            )
                        } else {
                            let env_vars =
                                std::env::vars()
                                    .collect::<Vec<_>>()
                                    .iter()
                                    .cloned()
                                    .map(|t| t.0)
                                    .collect::<HashSet<_>>();

                            let has_correct_env_creds =
                                env_vars.contains("AWS_ACCESS_KEY_ID")
                                    && env_vars.contains("AWS_SECRET_ACCESS_KEY");

                            if !has_correct_env_creds {
                                println!(
                                    "Error: notifier {:?} ({:?}) has no key and secret set, defaulting to env creds.",
                                    &notifier_id,
                                    &notifier.notifier_type,
                                );

                                println!(
                                    "Error: notifier {:?} ({:?}) requires env (AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY) to be set.",
                                    &notifier_id,
                                    &notifier.notifier_type,
                                );

                                exit(1);
                            }

                            let cred =
                                credential::EnvironmentProvider::default();

                            (
                                rusoto_sts::StsClient::new_with(
                                    HttpClient::new()?,
                                    cred.clone(),
                                    region.clone(),
                                ),
                                rusoto_sns::SnsClient::new_with(
                                    HttpClient::new()?,
                                    cred,
                                    region,
                                )
                            )
                        }
                    };

                    match sts_client.get_caller_identity(GetCallerIdentityRequest {})
                        .await {
                        Ok(GetCallerIdentityResponse {
                               account,
                               arn,
                               user_id,
                           }) => {
                            if args.verbose() {
                                println!(
                                    "Configured notifier (type: {}, key: {}, iid: {})..",
                                    "telegram",
                                    &notifier.auth.key.as_ref().unwrap(),
                                    notifier_id,
                                );

                                println!(
                                    "Using STS identity (account: {}, arn: {}, user_id: {})..",
                                    account.unwrap_or("".to_string()),
                                    arn.unwrap_or("".to_string()),
                                    user_id.unwrap_or("".to_string()),
                                );
                            }
                        }
                        Err(err) => {
                            println!(
                                "Error: notifier {:?} ({:?}) could not validate AWS credentials.",
                                &notifier_id,
                                &notifier.notifier_type,
                            );

                            if args.verbose() {
                                println!(
                                    "Error: {:?}",
                                    err,
                                );
                            }

                            exit(1);
                        }
                    }

                    for channel in notifier.channels.iter() {
                        let has_phone = channel.phone.is_some();
                        let has_target = channel.target_arn.is_some();
                        let has_topic = channel.topic_arn.is_some();

                        if has_phone {
                            let opt_out_check = sns_client.check_if_phone_number_is_opted_out(
                                CheckIfPhoneNumberIsOptedOutInput {
                                    phone_number: channel.phone.as_ref().unwrap().clone(),
                                }
                            ).await?;

                            if opt_out_check.is_opted_out.is_some()
                                && opt_out_check.is_opted_out.unwrap() == true {
                                println!(
                                    "Error: notifier {} ({:?}) channel {:?}: {:?} has explicitly opted out from receing SMS via SNS.",
                                    &notifier_id,
                                    &notifier.notifier_type,
                                    &channel.name,
                                    channel.phone.as_ref().unwrap(),
                                );

                                exit(1);
                            }
                        }

                        let var_cnt = has_phone as i8 + has_target as i8 + has_topic as i8;

                        if var_cnt > 1 || var_cnt == 0 {
                            println!(
                                "Error: notifier {} ({:?}) channel {:?} must specify at least and at most one of phone, target_arn or topic_arn.",
                                &notifier_id,
                                &notifier.notifier_type,
                                &channel.name,
                            );

                            exit(1);
                        }

                        channels.insert(
                            channel.name.clone(),
                            (
                                notifier_id,
                                ZuseChannelType::Sns(
                                    channel.phone.clone(),
                                    channel.target_arn.clone(),
                                    channel.topic_arn.clone(),
                                ),
                            ),
                        );
                    }

                    notifiers.push(
                        ZuseNotifyType::Sns(
                            sns_client,
                        ),
                    );
                },
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
        msg: ZuseJobMessage,
    ) -> Result<()> {
        let notifier =
            self.notifiers
                .get(channel.0)
                .unwrap();

        match notifier {
            ZuseNotifyType::Telegram(api) => {
                let mut tg_msg = {
                    let chat_id = match &channel.1 {
                        ZuseChannelType::Telegram(chat_id)
                        => &*chat_id,
                        _ => "",
                    };

                    telegram_bot::ChannelId::new(
                        chat_id.clone().parse::<i64>().unwrap()
                    )
                        .text(
                            msg.build(
                                &self.args,
                                &channel.0,
                            ).1,
                        )
                };

                tg_msg
                    .parse_mode(telegram_bot::types::ParseMode::Html)
                    .disable_preview();

                api.send(
                    tg_msg,
                ).await?;
            }
            ZuseNotifyType::Sns(client) => {
                match &channel.1 {
                    ZuseChannelType::Sns(phone, target, topic) => {
                        let (sender_id, subject, message) = msg.build(
                            &self.args,
                            &channel.0,
                        );

                        let mut sns_attr: HashMap<String, MessageAttributeValue> = HashMap::new();

                        sns_attr.insert(
                            "DefaultSenderID".into(),
                            MessageAttributeValue {
                                data_type: "String".into(),
                                string_value: Some(sender_id.into()),
                                binary_value: None,
                            },
                        );

                        sns_attr.insert(
                            "DefaultSMSType".into(),
                            MessageAttributeValue {
                                data_type: "String".into(),
                                string_value: Some("Transactional".into()),
                                binary_value: None,
                            },
                        );

                        let publish_input = rusoto_sns::PublishInput {
                            message,
                            subject: Some(subject),
                            message_attributes: Some(sns_attr),
                            message_structure: None,
                            phone_number: phone.clone(),
                            target_arn: target.clone(),
                            topic_arn: topic.clone(),
                        };

                        client.publish(
                            publish_input,
                        ).await?;
                    }
                    _ => {}
                }
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
            let mut client = reqwest::Client::new()
                .get(&test.url.clone());

            if test.timeout.is_some() {
                client = client
                    .timeout(
                        Duration::from_secs(
                            test.timeout
                                .as_ref()
                                .unwrap()
                                .clone(),
                        ),
                    );
            }

            let req = client.send().await;

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

            let dump_used = if dump_req.0 { true } else { false };

            let dump_url = if dump_req.0 {
                format!(
                    "{}#{}",
                    &dump_req.1.as_ref().unwrap(),
                    base64::encode(&serialized_req),
                )
            } else {
                "".to_string()
            };

            let dump_html = if dump_req.0 {
                format!(
                    "<a href='{}'>view dump</a>, ",
                    &dump_url,
                )
            } else {
                "".to_string()
            };

            let msg = ZuseJobMessage {
                test_id: test_id.clone(),
                test_name: test.name.clone(),
                test_url: test.url.clone(),
                dump_html,
                dump_url,
                dump_used,
                time_state_lasted: jsm.last_state_lasted,
                state: JobSMStates::Normative,
            };

            if jsm.state_changed() {
                match jsm.state {
                    JobSMStates::Failure => {
                        tx.send(
                            msg.with_state(
                                jsm.state.clone(),
                            ),
                        ).await;
                    },
                    JobSMStates::Recovery => {
                        tx.send(
                            msg.with_state(
                                jsm.state.clone(),
                            ),
                        ).await;

                        jsm.normative();
                    },
                    _ => {},
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
                .get(msg.test_id)
                .unwrap();

        for notify_channel in testcase.notify.iter() {
            if let Some(channel)
            = self.channels.get(notify_channel) {
                let send_op = self.try_send(
                    &channel,
                    msg.clone(),
                ).await;

                send_op
                    .map_err(|err|
                        println!(
                            "WARNING: SENDING MESSAGE VIA CHANNEL {:?} FAILED: {:?}",
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
    ).await?;

    prof1t.run().await?;

    Ok(())
}
