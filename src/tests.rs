use crate::{
    config::{Config, Transport},
    monitor::{Event, Worker},
    nut::{Client, Error, Power, Values, number, power},
    policy::{Decision, Policy},
};
use native_tls::{Certificate, Identity, TlsAcceptor};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

fn listener() -> TcpListener {
    TcpListener::bind("127.0.0.1:0").unwrap()
}

fn expect(stream: &mut impl Read, command: &str) {
    let mut line = String::new();
    BufReader::with_capacity(1, stream)
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, command);
}

fn reply(stream: &mut impl Write, message: &str) {
    stream.write_all(message.as_bytes()).unwrap();
    stream.flush().unwrap();
}

fn plain(server: impl FnOnce(TcpStream) + Send + 'static) -> (Client, thread::JoinHandle<()>) {
    let listener = listener();
    let addr = listener.local_addr().unwrap();
    let thread = thread::spawn(move || server(listener.accept().unwrap().0));
    let client = Client::new(
        TcpStream::connect(addr).unwrap(),
        "localhost",
        Transport::Plain,
        Arc::default(),
    )
    .unwrap();
    (client, thread)
}

#[test]
fn discovery_telemetry_and_escaping() {
    let (mut client, server) = plain(|mut stream| {
        expect(&mut stream, "LIST UPS\n");
        reply(
            &mut stream,
            "BEGIN LIST UPS\r\nUPS ups-1 \"Device \\\"A\\\" with \\\\ path\"\r\nUPS ups-2 \"\"\r\nEND LIST UPS\r\n",
        );
        expect(&mut stream, "LIST VAR \"ups-1\"\n");
        reply(
            &mut stream,
            "BEGIN LIST VAR ups-1\nVAR ups-1 ups.status \"OL CHRG\"\nVAR ups-1 battery.charge \"92.1\"\nVAR ups-1 battery.runtime \"unknown\"\nEND LIST VAR ups-1\n",
        );
    });
    assert_eq!(client.discover().unwrap(), ["ups-1", "ups-2"]);
    let values = client.poll("ups-1").unwrap();
    assert_eq!(power(&values), Power::Line);
    assert_eq!(number(&values, "battery.charge"), Some(92.1));
    assert_eq!(number(&values, "battery.runtime"), None);
    assert_eq!(number(&values, "output.voltage"), None);
    assert_eq!(client.poll("ups\nLOGOUT"), Err(Error::Protocol));
    server.join().unwrap();
}

#[test]
fn reads_different_ups_vendors_and_optional_nut_fields() {
    let models = [
        (
            "apc",
            "APC",
            &[
                ("ups.mfr", "APC"),
                ("input.frequency", "59.9"),
                ("battery.voltage", "27.2"),
                ("ups.realpower", "138"),
            ][..],
        ),
        (
            "eaton",
            "Eaton",
            &[
                ("device.mfr", "Eaton"),
                ("output.frequency", "50,1"),
                ("output.realpower", "324"),
                ("battery.voltage", "54.6"),
            ][..],
        ),
        (
            "cyberpower",
            "CyberPower",
            &[
                ("ups.mfr", "CyberPower"),
                ("ups.load", "0"),
                ("battery.voltage.nominal", "24"),
                ("ups.power", "250"),
            ][..],
        ),
    ];
    let (mut client, server) = plain(move |mut stream| {
        expect(&mut stream, "LIST UPS\n");
        let mut list = "BEGIN LIST UPS\n".to_owned();
        for (id, make, _) in models {
            list.push_str(&format!("UPS {id} \"{make}\"\n"));
        }
        list.push_str("END LIST UPS\n");
        reply(&mut stream, &list);
        for (id, _, fields) in models {
            expect(&mut stream, &format!("LIST VAR \"{id}\"\n"));
            let mut reply_text = format!("BEGIN LIST VAR {id}\n");
            for (key, val) in fields {
                reply_text.push_str(&format!("VAR {id} {key} \"{val}\"\n"));
            }
            reply_text.push_str(&format!("END LIST VAR {id}\n"));
            reply(&mut stream, &reply_text);
        }
    });
    assert_eq!(client.discover().unwrap(), ["apc", "eaton", "cyberpower"]);
    for (id, make, _) in models {
        let values = client.poll(id).unwrap();
        assert_eq!(
            crate::nut::value(&values, &["device.mfr", "ups.mfr"]),
            Some(make)
        );
        match id {
            "apc" => {
                assert_eq!(crate::nut::frequency_key(&values), "input.frequency");
                assert_eq!(number(&values, "battery.voltage"), Some(27.2));
            }
            "eaton" => {
                assert_eq!(crate::nut::frequency_key(&values), "output.frequency");
                assert_eq!(number(&values, "output.frequency"), Some(50.1));
                assert_eq!(crate::nut::real_power(&values), Some(324.0));
                assert_eq!(number(&values, "battery.voltage"), Some(54.6));
            }
            _ => {
                assert_eq!(number(&values, "ups.load"), Some(0.0));
                assert_eq!(number(&values, "battery.voltage"), None);
                assert_eq!(crate::nut::real_power(&values), None);
            }
        }
    }
    server.join().unwrap();
}

#[test]
fn parses_nut_hash_escapes_and_continuations() {
    let (mut client, server) = plain(|mut stream| {
        expect(&mut stream, "LIST VAR \"fixture\\#one\"\n");
        reply(
            &mut stream,
            concat!(
                "BEGIN LIST VAR \"fixture\\#one\"\n",
                "VAR \"fixture\\#one\" ups.model \"Lab\\#1, \\\n",
                "room = east\" # trailing note\n",
                "END LIST VAR \"fixture\\#one\"\n",
            ),
        );
    });
    let values = client.poll("fixture#one").unwrap();
    assert_eq!(values["ups.model"], "Lab#1, room = east");
    server.join().unwrap();
}

#[test]
fn malformed_cross_device_oversized_and_stale_responses() {
    let oversized = format!("BEGIN LIST VAR u\nVAR u a \"{}\"\n", "z".repeat(8192));
    for (response, expected) in [
        ("ERR DATA-STALE\n", Error::Stale),
        (
            "BEGIN LIST VAR u\nVAR other ups.status \"OB\"\nEND LIST VAR u\n",
            Error::Protocol,
        ),
        (
            "BEGIN LIST VAR u\nVAR u ups.status \"OL\"\nVAR u ups.status \"OB\"\nEND LIST VAR u\n",
            Error::Protocol,
        ),
        (
            "BEGIN LIST VAR u\nVAR u ups.status \"unterminated\n",
            Error::Protocol,
        ),
        ("BEGIN LIST VAR u\nEND LIST VAR other\n", Error::Protocol),
        (&oversized, Error::Protocol),
    ] {
        let response = response.to_owned();
        let (mut client, server) = plain(move |mut stream| {
            expect(&mut stream, "LIST VAR \"u\"\n");
            reply(&mut stream, &response);
        });
        assert_eq!(client.poll("u"), Err(expected));
        server.join().unwrap();
    }
}

#[test]
fn refusal_to_upgrade_never_sends_credentials_or_other_plain_commands() {
    let listener = listener();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        expect(&mut stream, "STARTTLS\n");
        reply(&mut stream, "ERR FEATURE-NOT-SUPPORTED\n");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    assert!(matches!(
        Client::new(
            TcpStream::connect(addr).unwrap(),
            "localhost",
            Transport::StartTls,
            Arc::default()
        ),
        Err(Error::Tls)
    ));
    server.join().unwrap();

    let (mut client, server) = plain(|mut stream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    assert_eq!(client.login("user", "secret"), Err(Error::Authentication));
    drop(client);
    server.join().unwrap();
}

fn tls_fixture(transport: Transport, host: &str, trust: bool, accept: Option<bool>) {
    let listener = listener();
    let addr = listener.local_addr().unwrap();
    let identity = Identity::from_pkcs12(
        include_bytes!("../tests/fixtures/server.pfx"),
        "rustnut-fixture",
    )
    .unwrap();
    let acceptor = TlsAcceptor::new(identity).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        if transport == Transport::StartTls {
            expect(&mut stream, "STARTTLS\n");
            reply(&mut stream, "OK STARTTLS\n");
        }
        if let Some(accept) = accept {
            let mut stream = acceptor.accept(stream).unwrap();
            expect(&mut stream, "USERNAME \"monitor user\"\n");
            reply(&mut stream, "OK\n");
            expect(&mut stream, "PASSWORD \"quote \\\" and slash \\\\\"\n");
            if accept {
                reply(&mut stream, "OK\n");
                expect(&mut stream, "LIST UPS\n");
                reply(
                    &mut stream,
                    "BEGIN LIST UPS\nUPS u \"TLS fixture\"\nEND LIST UPS\n",
                );
            } else {
                reply(&mut stream, "ERR ACCESS-DENIED\n");
            }
        } else {
            let _ = acceptor.accept(stream);
        }
    });
    let client = Client::connect(
        TcpStream::connect(addr).unwrap(),
        host,
        transport,
        Arc::default(),
        |builder| {
            if trust {
                builder.add_root_certificate(
                    Certificate::from_der(include_bytes!("../tests/fixtures/ca.der")).unwrap(),
                );
            }
        },
    );
    if let Some(accept) = accept {
        let mut client = client.unwrap();
        let auth = client.login("monitor user", "quote \" and slash \\");
        if accept {
            auth.unwrap();
            assert_eq!(client.discover().unwrap(), ["u"]);
        } else {
            assert_eq!(auth, Err(Error::Authentication));
        }
    } else {
        assert!(matches!(client, Err(Error::Tls)));
    }
    server.join().unwrap();
}

#[test]
fn verified_starttls_and_direct_tls_with_authentication() {
    tls_fixture(Transport::StartTls, "localhost", true, Some(true));
    tls_fixture(Transport::DirectTls, "localhost", true, Some(false));
}

#[test]
fn tls_rejects_untrusted_root_and_wrong_hostname() {
    tls_fixture(Transport::StartTls, "localhost", false, None);
    tls_fixture(Transport::DirectTls, "wrong.invalid", true, None);
}

#[test]
fn monitor_reconnects_then_stops_without_an_extra_poll() {
    let listener = listener();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        drop(listener.accept().unwrap());
        let (mut stream, _) = listener.accept().unwrap();
        expect(&mut stream, "LIST UPS\n");
        reply(
            &mut stream,
            "BEGIN LIST UPS\nUPS u \"Test UPS\"\nEND LIST UPS\n",
        );
        expect(&mut stream, "LIST VAR \"u\"\n");
        reply(
            &mut stream,
            "BEGIN LIST VAR u\nVAR u ups.status \"OL\"\nEND LIST VAR u\n",
        );
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(stream.read(&mut [0]).unwrap(), 0);
    });
    let cfg = Config {
        host: "127.0.0.1".into(),
        port,
        transport: Transport::Plain,
        ..Config::default()
    };
    let (sender, events) = mpsc::channel();
    let worker = Worker::start(cfg, Zeroizing::new(String::new()), move |event| {
        let _ = sender.send(event);
    })
    .unwrap();
    let next = || events.recv_timeout(Duration::from_secs(6)).unwrap();
    assert!(matches!(next(), Event::Connecting));
    assert!(matches!(next(), Event::Failed(_, 2)));
    assert!(matches!(next(), Event::Connecting));
    assert!(matches!(next(), Event::Devices(devices) if devices == ["u"]));
    assert!(
        matches!(next(), Event::Data { ups, values, at } if ups == "u" && power(&values) == Power::Line && at.elapsed() < Duration::from_secs(3))
    );
    drop(worker);
    server.join().unwrap();
}

fn rules() -> Config {
    Config {
        host: "localhost".into(),
        ups: "u".into(),
        shutdown: true,
        ..Config::default()
    }
}

fn snapshot(status: &str, charge: &str) -> Values {
    Values::from([
        ("ups.status".into(), status.into()),
        ("battery.charge".into(), charge.into()),
    ])
}

#[test]
fn old_power_settings_default_to_interactive_shutdown() {
    let cfg: Config = serde_json::from_str(r#"{"power":{"immediate":true}}"#).unwrap();
    assert!(cfg.power.immediate);
    assert!(!cfg.power.force_close);
    assert!(!cfg.power.on_battery);
}

#[test]
fn system_theme_is_default_and_old_theme_choices_are_preserved() {
    assert!(Config::default().system_theme);
    for dark in [false, true] {
        let old: Config = serde_json::from_value(serde_json::json!({ "dark": dark })).unwrap();
        assert!(!old.system_theme);
        assert_eq!(old.dark, dark);
    }
    let saved: Config =
        serde_json::from_value(serde_json::to_value(Config::default()).unwrap()).unwrap();
    assert!(saved.system_theme);
}

#[test]
fn battery_mode_can_act_on_first_fresh_report_without_thresholds() {
    let mut cfg = Config {
        charge: 0,
        battery_seconds: 0,
        transport: Transport::Plain,
        ..rules()
    };
    assert!(cfg.validate().is_err());
    cfg.power.on_battery = true;
    cfg.power.immediate = true;
    assert!(cfg.validate().is_ok());

    let start = Instant::now();
    let battery = snapshot("OB", "");
    let mut policy = Policy::default();
    assert_eq!(
        policy.sample(&cfg, "u", &battery, start, start + Duration::from_secs(4)),
        Decision::None
    );
    let mut sample = |second, ups, values: &Values| {
        let at = start + Duration::from_secs(second);
        policy.sample(&cfg, ups, values, at, at)
    };
    assert_eq!(sample(5, "other", &battery), Decision::None);
    assert_eq!(sample(10, "u", &snapshot("OB OL", "")), Decision::None);
    assert_eq!(sample(15, "u", &battery), Decision::ShutDown);
    assert_eq!(sample(20, "u", &battery), Decision::None);
    assert_eq!(sample(25, "u", &snapshot("OL", "")), Decision::None);
    assert_eq!(sample(30, "u", &battery), Decision::ShutDown);
}

#[test]
fn battery_mode_with_countdown_cancels_when_utility_power_returns() {
    let mut cfg = rules();
    cfg.power.on_battery = true;
    cfg.power.delay = 5;
    let start = Instant::now();
    let mut policy = Policy::default();
    let mut sample = |second, status| {
        let at = start + Duration::from_secs(second);
        policy.sample(&cfg, "u", &snapshot(status, ""), at, at)
    };
    assert_eq!(sample(0, "OB"), Decision::Warn);
    assert_eq!(sample(5, "OL"), Decision::None);
    assert_eq!(sample(10, "OB"), Decision::Warn);
    assert_eq!(sample(15, "OB"), Decision::ShutDown);
}

#[test]
fn shutdown_requires_repeated_fresh_data_and_grace() {
    let cfg = rules();
    let mut policy = Policy::default();
    let now = Instant::now();
    let data = snapshot("OB LB", "13");
    let mut sample = |seconds| {
        let at = now + Duration::from_secs(seconds);
        policy.sample(&cfg, "u", &data, at, at)
    };
    assert_eq!(sample(0), Decision::None);
    assert_eq!(sample(5), Decision::Warn);
    for seconds in [10, 15, 20, 25, 30, 34] {
        assert_eq!(sample(seconds), Decision::None);
    }
    assert_eq!(sample(35), Decision::ShutDown);
    assert_eq!(sample(40), Decision::None);
}

#[test]
fn cancellation_rearms_only_on_line_and_disconnect_resets_grace() {
    let cfg = rules();
    let start = Instant::now();
    let low = snapshot("OB", "4");
    let mut policy = Policy::default();
    let sample = |policy: &mut Policy, seconds, data: &Values| {
        let at = start + Duration::from_secs(seconds);
        policy.sample(&cfg, "u", data, at, at)
    };
    assert_eq!(sample(&mut policy, 0, &low), Decision::None);
    assert_eq!(sample(&mut policy, 5, &low), Decision::Warn);
    policy.cancel();
    policy.interrupted();
    assert_eq!(sample(&mut policy, 40, &low), Decision::None);
    assert_eq!(sample(&mut policy, 45, &low), Decision::None);
    assert_eq!(
        sample(&mut policy, 50, &snapshot("OL", "7")),
        Decision::None
    );
    assert_eq!(sample(&mut policy, 55, &low), Decision::None);
    assert_eq!(sample(&mut policy, 60, &low), Decision::Warn);
    assert!(policy.pending());
    policy.interrupted();
    assert!(!policy.pending());
    assert_eq!(sample(&mut policy, 65, &low), Decision::None);
}

#[test]
fn ambiguous_states_stale_samples_unauthorized_source_and_bad_charge_do_not_arm() {
    let mut cfg = rules();
    let start = Instant::now();
    let low = snapshot("OB", "1");
    let mut policy = Policy::default();
    for (time, values) in [
        (0, snapshot("OB OL", "1")),
        (5, snapshot("OB OFF", "1")),
        (10, snapshot("FSD", "0")),
        (15, snapshot("OB", "NaN")),
        (20, snapshot("OB", "-1")),
        (25, snapshot("OB", "")),
    ] {
        let at = start + Duration::from_secs(time);
        assert_eq!(policy.sample(&cfg, "u", &values, at, at), Decision::None);
        assert!(!policy.pending());
    }
    assert_eq!(
        policy.sample(&cfg, "u", &low, start, start + Duration::from_secs(60)),
        Decision::None
    );
    assert_eq!(
        policy.sample(&cfg, "other", &low, start, start),
        Decision::None
    );
    cfg.ups.clear();
    assert!(cfg.validate().is_err());
    assert_eq!(policy.sample(&cfg, "u", &low, start, start), Decision::None);
}

#[test]
fn power_actions_use_fresh_samples_for_every_supported_transport() {
    let start = Instant::now();
    let low = snapshot("OB", "1");
    for transport in [Transport::Plain, Transport::StartTls, Transport::DirectTls] {
        let mut cfg = Config {
            transport,
            ..rules()
        };
        assert!(cfg.validate().is_ok());
        let mut policy = Policy::default();
        for (seconds, decision) in [
            (0, Decision::None),
            (5, Decision::Warn),
            (10, Decision::None),
            (15, Decision::None),
            (20, Decision::None),
            (25, Decision::None),
            (30, Decision::None),
            (35, Decision::ShutDown),
        ] {
            let at = start + Duration::from_secs(seconds);
            assert_eq!(policy.sample(&cfg, "u", &low, at, at), decision);
        }
        cfg.shutdown = false;
        assert_eq!(
            Policy::default().sample(&cfg, "u", &low, start, start),
            Decision::None
        );
    }
}
#[test]
fn elapsed_battery_duration_works_with_unavailable_charge_and_detects_gaps() {
    let cfg = Config {
        charge: 0,
        battery_seconds: 10,
        ..rules()
    };
    let start = Instant::now();
    let values = snapshot("OB", "");
    let mut policy = Policy::default();
    let mut sample = |seconds| {
        let at = start + Duration::from_secs(seconds);
        policy.sample(&cfg, "u", &values, at, at)
    };
    assert_eq!(sample(0), Decision::None);
    assert_eq!(sample(5), Decision::None);
    assert_eq!(sample(10), Decision::Warn);
    assert_eq!(sample(60), Decision::None);
    assert!(!policy.pending());
}

#[test]
fn defaults_are_fail_safe_and_persist_without_password_text() {
    let cfg: Config = serde_json::from_str("{}").unwrap();
    assert!(!cfg.shutdown);
    assert_eq!(cfg.transport, Transport::StartTls);
    assert_eq!(
        serde_json::from_str::<Config>(&serde_json::to_string(&cfg).unwrap()).unwrap(),
        cfg
    );
    assert!(
        Config {
            user: "admin".into(),
            ..cfg
        }
        .validate()
        .is_err()
    );
}

#[test]
fn runtime_threshold_and_immediate_mode_require_two_qualifying_samples() {
    let mut cfg = Config {
        charge: 0,
        ..rules()
    };
    cfg.power.runtime = 120;
    cfg.power.immediate = true;
    cfg.power.extra_enabled = true;
    let now = Instant::now();
    let mut policy = Policy::default();
    let mut sample = |seconds, runtime: &str| {
        let at = now + Duration::from_secs(seconds);
        let mut values = snapshot("OB", "75");
        values.insert("battery.runtime".into(), runtime.into());
        policy.sample(&cfg, "u", &values, at, at)
    };
    assert_eq!(sample(0, "200"), Decision::None);
    assert_eq!(sample(5, "120"), Decision::None);
    assert_eq!(sample(10, "not available"), Decision::None);
    assert_eq!(sample(15, "119"), Decision::None);
    assert_eq!(sample(20, "118"), Decision::ShutDown);
}

#[test]
fn fsd_requires_opt_in_and_observes_both_delays() {
    let mut cfg = Config {
        charge: 0,
        ..rules()
    };
    let now = Instant::now();
    let mut policy = Policy::default();
    let fsd = snapshot("OL FSD", "");
    let mut sample = |cfg: &Config, seconds| {
        let at = now + Duration::from_secs(seconds);
        policy.sample(cfg, "u", &fsd, at, at)
    };
    assert!(cfg.validate().is_err());
    assert_eq!(sample(&cfg, 0), Decision::None);
    cfg.power.fsd = true;
    cfg.power.delay = 5;
    cfg.power.extra_delay = 7;
    cfg.power.extra_enabled = true;
    assert!(cfg.validate().is_ok());
    assert_eq!(sample(&cfg, 5), Decision::None);
    assert_eq!(sample(&cfg, 10), Decision::Warn);
    assert_eq!(sample(&cfg, 21), Decision::None);
    assert_eq!(sample(&cfg, 22), Decision::ShutDown);
}

#[test]
fn custom_telemetry_limits_override_nominal_frequency_and_validate_order() {
    let mut cfg = Config::default();
    cfg.limits.nominal_hz = 50;
    assert!(cfg.limits.flags("input.frequency", 60.0));
    assert!(!cfg.limits.flags("input.frequency", 55.0));
    cfg.limits.ranges[1] = crate::config::Range {
        min: Some(58.0),
        max: Some(62.0),
    };
    assert!(!cfg.limits.flags("input.frequency", 60.0));
    assert!(cfg.limits.flags("input.frequency", 57.0));
    assert!(!cfg.limits.flags("output.frequency", 57.0));
    cfg.limits.ranges[1].min = Some(63.0);
    assert!(cfg.validate().is_err());
}

#[test]
fn default_limits_are_populated_and_can_be_suppressed() {
    let mut limits = crate::config::Limits::default();
    assert!(
        limits
            .ranges
            .iter()
            .all(|r| r.min.is_some() && r.max.is_some())
    );
    assert!(limits.flags("input.voltage", 275.0));
    limits.enabled = false;
    assert!(!limits.flags("input.voltage", 275.0));
    assert_eq!(limits.ranges[0].max, Some(264.0));
    limits.ranges[0].min = None;
    limits.ranges[0].max = Some(76.0);
    limits.complete();
    assert_eq!(limits.ranges[0].min, Some(76.0));
}

#[test]
fn voltage_accepts_decimal_comma_and_identity_uses_available_aliases() {
    let values = Values::from([
        ("input.voltage".into(), " 230,0 ".into()),
        ("output.voltage".into(), "230.1".into()),
        ("battery.voltage.nominal".into(), "12".into()),
        ("device.mfr".into(), " unknown ".into()),
        ("ups.mfr".into(), "POWERCOM".into()),
        ("device.serial".into(), "".into()),
        ("ups.serial".into(), " 00-ABC ".into()),
    ]);
    assert_eq!(number(&values, "input.voltage"), Some(230.0));
    assert_eq!(number(&values, "output.voltage"), Some(230.1));
    assert_eq!(number(&values, "battery.voltage"), None);
    assert_eq!(
        crate::nut::value(&values, &["device.mfr", "ups.mfr"]),
        Some("POWERCOM")
    );
    assert_eq!(
        crate::nut::value(&values, &["device.serial", "ups.serial"]),
        Some("00-ABC")
    );
}
