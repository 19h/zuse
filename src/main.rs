#![feature(async_closure)]

use anyhow::{Result, Context};
use clap::{Arg, App};
use serde_derive::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::process::exit;
use std::time::Duration;
use telegram_bot::prelude::*;
use rusoto_core::{Region, HttpClient, credential};
use rusoto_sns::{Sns, CheckIfPhoneNumberIsOptedOutInput, MessageAttributeValue};
use rusoto_sts::{Sts, GetCallerIdentityRequest, GetCallerIdentityResponse};
use futures::Future;
use tokio::net::TcpStream;
use std::net::{IpAddr, SocketAddr};

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
    alert_subject: Option<String>,
    alert_plain: Option<String>,
    alert_html: Option<String>,
    resolve_subject: Option<String>,
    resolve_plain: Option<String>,
    resolve_html: Option<String>,
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
struct ZuseConfigTestExpecations {
    text: Option<String>,
    status: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum ZuseConfigTestType {
    #[serde(rename = "http_ok")]
    HttpOk,

    #[serde(rename = "tcp_ok")]
    TcpOk,

    #[serde(rename = "http_match")]
    HttpMatch,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigTest {
    #[serde(rename = "type")]
    test_type: ZuseConfigTestType,

    name: String,
    target: String,

    expect: Option<ZuseConfigTestExpecations>,

    notify: Option<Vec<String>>,
    notify_groups: Option<Vec<String>>,

    // can be overriden by defaults
    retries: Option<u64>,
    recovery: Option<u64>,
    interval: Option<u64>,
    timeout: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigInternal {
    dump_prefix_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigDefaults {
    retries: Option<u64>,
    recovery: Option<u64>,
    interval: Option<u64>,
    timeout: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfigNotifyGroups {
    name: String,
    notify: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ZuseConfig {
    config: Option<ZuseConfigInternal>,
    defaults: Option<ZuseConfigDefaults>,
    notifiers: Vec<ZuseConfigNotifier>,
    notify_groups: Option<Vec<ZuseConfigNotifyGroups>>,
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

type ZuseNotifyGroup = Vec<String>;
type ZuseNotifyGroupMap = HashMap<String, ZuseNotifyGroup>;

const DEFAULT_SENDER_ID: &'static str = "NOTICE";

const DEFAULT_MSG_TMPL_ALRT_SUBJECT: &'static str = "ALRT {{test_name}}";
const DEFAULT_MSG_TMPL_ALRT_PLAIN: &'static str = "ALRT Uptime checks failed on '{{test_name}}'. (url: {{test_url}})";
const DEFAULT_MSG_TMPL_ALRT_HTML: &'static str = "<b>ALRT</b> Uptime checks failed on '{{test_name}}'. (url: {{test_url}})";

const DEFAULT_MSG_TMPL_RSLV_SUBJECT: &'static str = "RSVL {{test_name}}";
const DEFAULT_MSG_TMPL_RSLV_PLAIN: &'static str = "RSLV Uptime checks recovered on '{{test_name}}'. (duration={{time_state_lasted}}s, url: {{test_url}})";
const DEFAULT_MSG_TMPL_RSLV_HTML: &'static str = "<b>RSLV</b> Uptime checks recovered on '{{test_name}}'. (duration={{time_state_lasted}}s, url: {{test_url}})";

#[derive(Debug, Clone)]
enum ZuseRunnerStatus {
    Ok,
    Failure,
}

impl Into<ZuseRunnerStatus> for bool {
    #[inline(always)]
    fn into(self) -> ZuseRunnerStatus {
        if self {
            ZuseRunnerStatus::Ok
        } else {
            ZuseRunnerStatus::Failure
        }
    }
}

#[derive(Debug, Clone)]
struct ZuseTestResult {
    status: ZuseRunnerStatus,
    debug_dump: Option<String>,
}

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
                            t.alert_subject.clone(),
                            t.alert_html.clone(),
                            t.alert_plain.clone(),
                            t.resolve_subject.clone(),
                            t.resolve_html.clone(),
                            t.resolve_plain.clone(),
                        )
                );

        match &self.state {
            JobSMStates::Failure => {
                (
                    tmpl_cstm.0.unwrap_or(DEFAULT_MSG_TMPL_ALRT_SUBJECT.to_string()),
                    tmpl_cstm.1.unwrap_or(DEFAULT_MSG_TMPL_ALRT_HTML.to_string()),
                    tmpl_cstm.2.unwrap_or(DEFAULT_MSG_TMPL_ALRT_PLAIN.to_string())
                )
            },
            JobSMStates::Recovery => {
                (
                    tmpl_cstm.3.unwrap_or(DEFAULT_MSG_TMPL_RSLV_SUBJECT.to_string()),
                    tmpl_cstm.4.unwrap_or(DEFAULT_MSG_TMPL_RSLV_HTML.to_string()),
                    tmpl_cstm.5.unwrap_or(DEFAULT_MSG_TMPL_RSLV_PLAIN.to_string())
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
        //       a platform plain or html, impl it here
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

    notify_groups: ZuseNotifyGroupMap,
}

impl Zuse {
    async fn new(
        mut args: ZuseArgs,
    ) -> Result<Zuse> {
        let mut notifiers = Vec::new();

        let mut channels: ZuseChannelMap = HashMap::new();
        let mut notify_groups = HashMap::new();

        /*
            VALIDATE CONFIGURATION AND TEST LAYOUT
        */

        {
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

                        let api = telegram_bot::Api::new(
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

                        let (sts_client, sns_client) = {
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
        }

        /*
            VERIFY AND CONSTRUCT NOTIFY GROUPS
        */

        {
            if args.config.notify_groups.is_some() {
                for notify_group in args.config.notify_groups.as_ref().unwrap() {
                    let mut notify_group_channel_refs = Vec::new();

                    for notify in notify_group.notify.iter() {
                        if !channels.contains_key(notify) {
                            println!(
                                "Error: notify group '{}' references inexistent channel: {}",
                                &notify_group.name,
                                notify,
                            );

                            exit(1);
                        }

                        notify_group_channel_refs.push(
                            notify.clone(),
                        );
                    }

                    notify_groups.insert(
                        notify_group.name.clone(),
                        notify_group_channel_refs,
                    );
                }
            }
        }

        /*
            VERIFY TEST NOTIFY TARGETS AND NOTIFY GROUPS
        */

        {
            for test in args.config.tests.iter() {
                if test.notify.is_some() {
                    for notify in test.notify.as_ref().unwrap().iter() {
                        if !channels.contains_key(notify) {
                            println!(
                                "Error: '{}' (type: {:?}) references inexistent channel: {}",
                                test.name,
                                test.test_type,
                                notify,
                            );

                            exit(1);
                        }
                    }
                }

                if test.notify_groups.is_some() {
                    for notify_group in test.notify_groups.as_ref().unwrap().iter() {
                        if !notify_groups.contains_key(notify_group) {
                            println!(
                                "Error: '{}' (type: {:?}) references inexistent notify group: {}",
                                test.name,
                                test.test_type,
                                notify_group,
                            );

                            exit(1);
                        }
                    }
                }
            }
        }

        /*
            VALIDATE TEST TYPE REQUIREMENTS
        */

        {
            for test in args.config.tests.iter() {
                match test.test_type {
                    ZuseConfigTestType::HttpOk => {
                        if url::Url::parse(&test.target).is_ok() {
                            continue;
                        }

                        println!(
                            "Error: '{}' (type: {:?}) requires a valid URL as target. Got: {}",
                            test.name,
                            test.test_type,
                            &test.target,
                        );

                        exit(1);
                    },
                    ZuseConfigTestType::HttpMatch => {
                        if !url::Url::parse(&test.target).is_ok() {
                            println!(
                                "Error: '{}' (type: {:?}) requires a valid URL as target. Got: {}",
                                test.name,
                                test.test_type,
                                &test.target,
                            );

                            exit(1);
                        }

                        if test.expect.is_none() {
                            println!(
                                "Error: '{}' (type: {:?}) requires expectations.",
                                test.name,
                                test.test_type,
                            );

                            exit(1);
                        }

                        let expectations = &test.expect.as_ref().unwrap();

                        if expectations.status.is_none() && expectations.text.is_none() {
                            println!(
                                "Error: '{}' (type: {:?}) either status or text expectations.",
                                test.name,
                                test.test_type,
                            );

                            exit(1);
                        }

                        let parsed_status = hyper::http::status::StatusCode::from_u16(
                            expectations.status.as_ref().unwrap().clone(),
                        );

                        if parsed_status.is_err() {
                            println!(
                                "Error: '{}' (type: {:?}) has expects invalid status. Got: {}",
                                test.name,
                                test.test_type,
                                expectations.status.as_ref().unwrap(),
                            );

                            exit(1);
                        }
                    },
                    ZuseConfigTestType::TcpOk => {
                        if test.target.parse::<std::net::SocketAddr>().is_ok() {
                            continue;
                        }

                        println!(
                            "Error: '{}' (type: {:?}) requires an IPv4 (RFC791) or IPv6 (RFC4291) and 16 bit port tuple as target. Got: {}",
                            test.name,
                            test.test_type,
                            &test.target,
                        );

                        exit(1);
                    }
                }
            }
        }

        /*
            VERIFY RETRY, RECOVERY AND INTERVAL, APPLY DEFAULS IF NEEDED
        */

        {
            let (
                def_retries,
                def_recovery,
                def_interval,
                def_timeout,
            ) =
                args.config
                    .defaults
                    .as_ref()
                    .map(|defaults|
                        (
                            defaults.retries,
                            defaults.recovery,
                            defaults.interval,
                            defaults.timeout,
                        )
                    )
                    .unwrap_or((None, None, None, None));

            for test in args.config.tests.iter_mut() {
                if test.retries.is_none() && def_retries.is_none() {
                    println!(
                        "Error: '{}' (type: {:?}) lacks 'retries', but no default was set.",
                        test.name,
                        test.test_type,
                    );

                    exit(1);
                }

                if test.retries.is_none() && def_retries.is_some() {
                    test.retries = def_retries.clone();
                }

                if test.recovery.is_none() && def_recovery.is_none() {
                    println!(
                        "Error: '{}' (type: {:?}) lacks 'recovery', but no default was set.",
                        test.name,
                        test.test_type,
                    );

                    exit(1);
                }

                if test.recovery.is_none() && def_recovery.is_some() {
                    test.recovery = def_recovery.clone();
                }

                if test.interval.is_none() && def_interval.is_none() {
                    println!(
                        "Error: '{}' (type: {:?}) lacks 'interval', but no default was set.",
                        test.name,
                        test.test_type,
                    );

                    exit(1);
                }

                if test.interval.is_none() && def_interval.is_some() {
                    test.interval = def_interval.clone();
                }

                if test.timeout.is_none() && def_timeout.is_none() {
                    println!(
                        "Error: '{}' (type: {:?}) lacks 'timeout', but no default was set.",
                        test.name,
                        test.test_type,
                    );

                    exit(1);
                }

                if test.timeout.is_none() && def_timeout.is_some() {
                    test.timeout = def_timeout.clone();
                }
            }
        }

        Ok(
            Zuse {
                args,

                channels,
                notifiers,
                notify_groups,
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

                    let (_, _, message) = msg.build(
                        &self.args,
                        &channel.0,
                    );

                    let chat_id =
                        chat_id
                            .clone()
                            .parse::<i64>()
                            .unwrap();

                    telegram_bot
                        ::ChannelId
                        ::new(chat_id)
                            .text(message)
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

    async fn test_runner_http_ok(
        (_test_id, test): &(usize, ZuseConfigTest),
    ) -> ZuseTestResult {
        let mut client = reqwest::Client::new()
            .get(&test.target.clone());

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

        let res = client.send().await;

        let status =
            res.is_ok()
                && res.as_ref()
                .unwrap()
                .status()
                .is_success();

        ZuseTestResult {
            status: status.into(),
            debug_dump: Some(format!("{:#?}", res)),
        }
    }

    async fn test_runner_http_match(
        (_test_id, test): &(usize, ZuseConfigTest),
    ) -> ZuseTestResult {
        let mut client = reqwest::Client::new()
            .get(&test.target.clone());

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

        let expectations = test.expect.as_ref().unwrap();

        let res = client.send().await;

        let test_success = loop {
            if !res.as_ref().is_ok() {
                break false;
            }

            let res = res.unwrap();

            let status = res.status().clone();

            if expectations.text.is_some() {
                let text = res.text().await;

                if text.is_ok() {
                    let text = text.as_ref().unwrap();

                    if !text.contains(expectations.text.as_ref().unwrap()) {
                        break false;
                    }
                }
            }

            if expectations.status.is_some() {
                let parsed_status =
                    hyper::http::status::StatusCode::from_u16(
                        expectations.status.as_ref().unwrap().clone(),
                    ).unwrap();

                if status != parsed_status {
                    break false;
                }
            }

            break true;
        };

        ZuseTestResult {
            status: test_success.into(),
            debug_dump: None,
        }
    }

    async fn test_runner_tcp_ok(
        (_test_id, test): &(usize, ZuseConfigTest),
    ) -> ZuseTestResult {
        let conn =
            TcpStream::connect(
                &test.target.parse::<SocketAddr>().unwrap(),
            ).await;

        ZuseTestResult {
            status: conn.is_ok().into(),
            debug_dump: None,
        }
    }

    async fn test_runner_instrument(
        args: &ZuseArgs,
        test_box: &(usize, ZuseConfigTest),
        mut tx: tokio::sync::mpsc::Sender<ZuseJobMessage>,
    ) -> Result<()> {
        let ref test_id = test_box.0;
        let ref test = test_box.1;

        let mut jsm = JobStateMachine::new(
            test.retries.unwrap().clone(),
            test.recovery.unwrap().clone(),
        );

        let dump_req = {
            args.config
                .config
                .as_ref()
                .map_or(
                    (false, None),
                    |config|
                        config
                            .dump_prefix_url
                            .as_ref()
                            .map_or(
                                (false, None),
                                |dpu| (true, Some(dpu)),
                            ),
                )
        };

        loop {
            let status =
                match test_box.1.test_type {
                    ZuseConfigTestType::HttpOk =>
                        Zuse::test_runner_http_ok(test_box).await,
                    ZuseConfigTestType::HttpMatch =>
                        Zuse::test_runner_http_match(test_box).await,
                    ZuseConfigTestType::TcpOk =>
                        Zuse::test_runner_tcp_ok(test_box).await,
                };

            match status.status {
                ZuseRunnerStatus::Ok => jsm.win(),
                ZuseRunnerStatus::Failure => jsm.loss(),
            };

            if args.debug() {
                println!(
                    "alive: {:?} changed: {} now: {:?} was: {:?} lasted: {}",
                    status.status,
                    jsm.state_changed(),
                    jsm.state,
                    jsm.last_state,
                    jsm.last_state_lasted,
                );
            }

            let dump_used = dump_req.0;

            let dump_url =
                if dump_used && status.debug_dump.is_some() {
                    format!(
                        "{}#{}",
                        &dump_req.1.as_ref().unwrap(),
                        base64::encode(
                            status
                                .debug_dump
                                .as_ref()
                                .unwrap(),
                        ),
                    )
                } else {
                    "".to_string()
                };

            let dump_html = if dump_used {
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
                test_url: test.target.clone(),
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
                                JobSMStates::Failure,
                            ),
                        ).await;
                    },
                    JobSMStates::Recovery => {
                        jsm.normative();

                        tx.send(
                            msg.with_state(
                                JobSMStates::Recovery,
                            ),
                        ).await;
                    },
                    _ => {},
                }
            }

            tokio::time::delay_for(
                Duration::from_secs(
                    test.interval.unwrap().clone(),
                ),
            ).await;
        }

        // unreachable
    }

    async fn test_runner(
        args: ZuseArgs,
        ref test_box: (usize, ZuseConfigTest),
        mut tx: tokio::sync::mpsc::Sender<ZuseJobMessage>,
    ) -> Result<()> {
        Zuse::test_runner_instrument(
            &args,
            test_box,
            tx,
        ).await?;

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

        let empty_vec = Vec::new();
        let empty_vec_ng = Vec::new();

        let test_channels =
            testcase
                .notify
                .as_ref()
                .unwrap_or(&empty_vec)
                .iter()
                .collect::<Vec<_>>();

        let test_notify_groups =
            testcase
                .notify_groups
                .as_ref()
                .map(|ngs|
                    ngs.iter()
                        .flat_map(|ng|
                            &*self.notify_groups
                                .get(ng)
                                .unwrap(),
                        )
                        .collect::<Vec<_>>()
                )
                .unwrap_or(empty_vec_ng);

        for notify_channel in test_channels.iter().chain(test_notify_groups.iter()) {
            if let Some(channel)
            = self.channels.get(*notify_channel) {
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
        App::new("zuse")
            .version(clap::crate_version!())
            .author("Kenan Sulayman <kenan@sig.dev>")
            .about("The flexible uptime bot, a descendant of the Rust async masterrace.")
            .arg(Arg::with_name("config")
                .short("c")
                .long("config")
                .value_name("CONFIG")
                .help("Config file")
                .takes_value(true)
                .default_value("tests.yml"))
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
