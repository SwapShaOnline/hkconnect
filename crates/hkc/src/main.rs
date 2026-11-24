//! `hkc`: control a Hyundai or Kia (India) car from the terminal.

mod bench;
mod output;
mod store;

use std::io::{BufRead, IsTerminal as _, Write as _};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use clap::{CommandFactory as _, Parser, Subcommand};
use hkconnect::{
    Brand, Celsius, Client, ClimateOptions, CommandId, CommandStatus, Config, Credentials,
    Destination, Vehicle,
};
use output::clean;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Car brand: hyundai (the default) or kia.
    #[arg(long, env = "HKC_BRAND", global = true)]
    brand: Option<Brand>,
    /// Which car, by name, VIN, or id (required when the account has several).
    #[arg(long, short = 'c', env = "HKC_VEHICLE", global = true)]
    vehicle: Option<String>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// Show API calls (never secrets).
    #[arg(long, short, global = true)]
    verbose: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List cars on the account.
    Vehicles,
    /// Show the car's state.
    Status {
        /// Wake the car for fresh data (slower; uses the 12V battery).
        #[arg(long)]
        refresh: bool,
    },
    /// Where the car is parked.
    Location,
    /// Equipment, service intervals, and odometer.
    Info,
    /// Lock the doors.
    Lock(Wait),
    /// Unlock the doors.
    Unlock {
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        wait: Wait,
    },
    /// Start the engine with climate control.
    Start {
        /// Cabin temperature in °C (14–29.5, in 0.5 steps).
        #[arg(long, short, default_value_t = 22.0)]
        temp: f32,
        /// Minutes to run (1–10).
        #[arg(long, short, default_value_t = 10)]
        minutes: u8,
        /// Windscreen defrost.
        #[arg(long)]
        defrost: bool,
        /// Rear window and mirror heating.
        #[arg(long)]
        rear_defrost: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        wait: Wait,
    },
    /// Stop a remote start (needs your PIN).
    Stop(Wait),
    /// Flash the hazard lights (needs your PIN; experimental).
    Hazard {
        /// Sound the horn too (asks for confirmation).
        #[arg(long)]
        horn: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        wait: Wait,
    },
    /// Trip history. With no options: this month's summary.
    Trips {
        /// A month, e.g. 2026-11.
        #[arg(long, conflicts_with = "day")]
        month: Option<String>,
        /// A day, e.g. 2026-11-17: lists that day's trips.
        #[arg(long)]
        day: Option<jiff::civil::Date>,
        /// With --day: show the driving events of trip N.
        #[arg(long, requires = "day")]
        trip: Option<usize>,
        /// With --trip and --json: include the trip's full GPS route (left
        /// out by default; it is most of the output).
        #[arg(long, requires = "trip")]
        route: bool,
    },
    /// Turn valet mode on or off (needs your PIN; experimental).
    Valet {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        wait: Wait,
    },
    /// Send a destination to the car's navigation (needs your PIN; experimental).
    Navigate {
        /// Name shown on the car's screen.
        #[arg(long)]
        name: String,
        #[arg(long, allow_negative_numbers = true)]
        lat: f64,
        #[arg(long, allow_negative_numbers = true)]
        lon: f64,
        /// Street address shown on the car's screen.
        #[arg(long, default_value = "")]
        address: String,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
        #[command(flatten)]
        wait: Wait,
    },
    /// Check the result of an earlier command.
    Check { id: String },
    /// Measure API latency with read-only calls (never wakes the car).
    Bench {
        /// Calls per endpoint.
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u16).range(1..=20))]
        runs: u16,
        /// Pause between calls, in milliseconds (protects the rate limit).
        #[arg(long, default_value_t = 1000)]
        gap_ms: u64,
    },
    /// Log in now and save the session.
    Login,
    /// Delete the saved session.
    Logout,
    /// Print a shell completion script.
    ///
    /// zsh:  hkc completions zsh > ~/.zfunc/_hkc  (with ~/.zfunc in fpath)
    /// bash: hkc completions bash > ~/.local/share/bash-completion/completions/hkc
    /// fish: hkc completions fish > ~/.config/fish/completions/hkc.fish
    #[command(verbatim_doc_comment)]
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
    /// Print the manual page (roff), e.g. `hkc man > hkc.1`.
    Man,
}

#[derive(clap::Args)]
struct Wait {
    /// Return as soon as the server accepts the command.
    #[arg(long)]
    no_wait: bool,
    /// Seconds to wait for the car to respond.
    #[arg(long, default_value_t = 90)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // These need no account, config or network.
    match &cli.command {
        Command::Completions { shell } => {
            clap_complete::generate(*shell, &mut Cli::command(), "hkc", &mut std::io::stdout());
            return ExitCode::SUCCESS;
        }
        Command::Man => {
            return match clap_mangen::Man::new(Cli::command()).render(&mut std::io::stdout()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        _ => {}
    }
    init_logging(cli.verbose);
    match run(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {}", clean(&format!("{e:#}")));
            ExitCode::FAILURE
        }
    }
}

fn init_logging(verbose: bool) {
    let filter = if verbose {
        "hkconnect=debug,hkc=debug"
    } else {
        "warn"
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}

async fn run(mut cli: Cli) -> Result<ExitCode> {
    let store = store::Store::open()?;
    let env = store.load_env();

    if matches!(cli.command, Command::Logout) {
        store.forget_session()?;
        println!("Saved session deleted.");
        return Ok(ExitCode::SUCCESS);
    }

    // `--brand` / `--vehicle` and the real environment (read by clap) win
    // over the `.env` file.
    let brand = match cli.brand {
        Some(brand) => brand,
        None => match env.var("HKC_BRAND") {
            Some(raw) => raw
                .parse()
                .map_err(|e: String| anyhow::anyhow!("HKC_BRAND in .env: {e}"))?,
            None => Brand::Hyundai,
        },
    };
    if cli.vehicle.is_none() {
        cli.vehicle = env.var("HKC_VEHICLE");
    }
    let credentials = credentials_from_env(&env)?;
    let mut client = Client::new(Config::india(brand), credentials.clone())?;
    if let Some(session) = store.load_session(brand, credentials.username()) {
        client = client.with_session(session);
    }

    let outcome = execute(&cli, &client).await;
    // Save even after a failed command: the login itself may have succeeded.
    let saved = match client.session().await {
        Some(session) => store.save_session(brand, credentials.username(), &session),
        None => Ok(()),
    };
    keep_outcome(outcome, saved)
}

/// The command's own result, whatever happened when saving the session.
///
/// Saving is best effort: a failure is only a warning. Turning "Unlock:
/// done" into an error here would invite a script to unlock again.
fn keep_outcome(outcome: Result<ExitCode>, saved: Result<()>) -> Result<ExitCode> {
    if let Err(e) = saved {
        eprintln!(
            "warning: {} (the next command will log in again)",
            clean(&format!("{e:#}"))
        );
    }
    outcome
}

async fn execute(cli: &Cli, client: &Client) -> Result<ExitCode> {
    if matches!(cli.command, Command::Login) {
        client.login().await?;
        println!("Logged in; session saved.");
        return Ok(ExitCode::SUCCESS);
    }
    let vehicles = client.vehicles().await.context("could not list vehicles")?;
    if matches!(cli.command, Command::Vehicles) {
        output::vehicles(&vehicles, cli.json)?;
        return Ok(ExitCode::SUCCESS);
    }
    let several = vehicles.len() > 1;
    let car = pick_vehicle(vehicles, cli.vehicle.as_deref())?;
    if read(cli, client, &car).await? {
        return Ok(ExitCode::SUCCESS);
    }
    // Validate and confirm before anything is sent.
    let request = prepare(&cli.command, &car)?;
    let id = send(client, &car, &request).await?;
    // The id, not the VIN: the VIN may be empty, and the id is what the
    // library has already checked to be plain ASCII.
    let check = if several {
        format!("hkc --vehicle {} check {id}", car.id)
    } else {
        format!("hkc check {id}")
    };
    await_outcome(client, &car, &id, &request, &check, cli.json).await
}

/// Run a read-only command. Returns `false` if the command isn't one.
async fn read(cli: &Cli, client: &Client, car: &Vehicle) -> Result<bool> {
    match &cli.command {
        Command::Bench { runs, gap_ms } => {
            let gap = Duration::from_millis(*gap_ms);
            let results = bench::run(client, car, usize::from(*runs), gap).await?;
            bench::print(&results, cli.json)?;
        }
        Command::Status { refresh } => {
            let status = if *refresh {
                client.refresh_status(car).await?
            } else {
                client.status(car).await?
            };
            output::status(car, &status, cli.json)?;
        }
        Command::Location => output::location(client.location(car).await?.as_ref(), cli.json)?,
        Command::Info => {
            let (caps, maintenance) =
                tokio::try_join!(client.capabilities(car), client.maintenance(car))?;
            output::info(car, &caps, &maintenance, cli.json)?;
        }
        Command::Trips {
            month,
            day,
            trip,
            route,
        } => {
            let route = *route;
            trips(client, car, month.as_deref(), *day, *trip, route, cli.json).await?;
        }
        Command::Check { id } => {
            let status = client.command_status(car, &CommandId::new(id)).await?;
            output::command_status(status, cli.json)?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

/// A remote command, validated and confirmed, ready to send.
struct Request<'a> {
    action: Action,
    wait: &'a Wait,
    label: &'static str,
}

enum Action {
    Lock,
    Unlock,
    Start(ClimateOptions),
    Stop,
    Hazard { horn: bool },
    Valet { on: bool },
    Navigate(Destination),
}

/// Check a remote command's options, then ask for confirmation where it
/// matters. Nothing is sent here.
fn prepare<'a>(command: &'a Command, car: &Vehicle) -> Result<Request<'a>> {
    let car_name = describe(car);
    let (action, wait, label) = match command {
        Command::Lock(wait) => (Action::Lock, wait, "Lock"),
        Command::Unlock { yes, wait } => {
            confirm(*yes, &format!("Unlock {car_name}?"))?;
            (Action::Unlock, wait, "Unlock")
        }
        Command::Start {
            temp,
            minutes,
            defrost,
            rear_defrost,
            yes,
            wait,
        } => {
            let options = ClimateOptions::new(Celsius::new(*temp)?)
                .with_duration_minutes(*minutes)?
                .with_defrost(*defrost)
                .with_rear_defrost(*rear_defrost);
            let question = format!(
                "Start the engine of {car_name} for {} min at {}?",
                options.duration_minutes(),
                options.temperature()
            );
            confirm(*yes, &question)?;
            (Action::Start(options), wait, "Remote start")
        }
        Command::Stop(wait) => (Action::Stop, wait, "Stop"),
        Command::Hazard { horn, yes, wait } => {
            if *horn {
                confirm(
                    *yes,
                    &format!("Sound the horn and flash the hazard lights of {car_name}?"),
                )?;
            }
            (Action::Hazard { horn: *horn }, wait, "Hazard lights")
        }
        Command::Valet { state, yes, wait } => {
            let on = state == "on";
            confirm(*yes, &format!("Turn valet mode {state} for {car_name}?"))?;
            (Action::Valet { on }, wait, "Valet mode")
        }
        Command::Navigate {
            name,
            lat,
            lon,
            address,
            yes,
            wait,
        } => {
            let destination = Destination::new(name.clone(), *lat, *lon)?.with_address(address);
            confirm(
                *yes,
                &format!("Send {name:?} ({lat}, {lon}) to the navigation of {car_name}?"),
            )?;
            (Action::Navigate(destination), wait, "Send destination")
        }
        _ => unreachable!(
            "reads, login, logout, vehicles, completions and man are handled before sending"
        ),
    };
    Ok(Request {
        action,
        wait,
        label,
    })
}

/// A car named unambiguously for confirmation questions.
fn describe(car: &Vehicle) -> String {
    format!("{:?} (VIN {})", car.name, clean(&car.vin))
}

/// Send a prepared command. If sending fails in a way that leaves it unclear
/// whether the server got it ([`hkconnect::Error::may_have_been_received`]),
/// say so: retrying blindly could unlock or start the car twice.
async fn send(client: &Client, car: &Vehicle, request: &Request<'_>) -> Result<CommandId> {
    let label = request.label;
    let sent = tokio::select! {
        result = dispatch(client, car, &request.action) => result,
        () = ctrl_c() => bail!(
            "{label}: interrupted while sending.\n\
             The server may have received the command. \
             Run `hkc status` to check before trying again."
        ),
    };
    match sent {
        Ok(id) => Ok(id),
        Err(e) if e.may_have_been_received() => bail!(
            "{label}: {}\n\
             The server may have received the command anyway. \
             Run `hkc status` to check before trying again.",
            output::error_chain(&e)
        ),
        Err(e) => Err(e.into()),
    }
}

async fn dispatch(client: &Client, car: &Vehicle, action: &Action) -> hkconnect::Result<CommandId> {
    match action {
        Action::Lock => client.lock(car).await,
        Action::Unlock => client.unlock(car).await,
        Action::Start(options) => client.start_climate(car, options).await,
        Action::Stop => client.stop_climate(car).await,
        Action::Hazard { horn } => client.hazard_lights(car, *horn).await,
        Action::Valet { on } => client.valet_mode(car, *on).await,
        Action::Navigate(destination) => client.send_destination(car, destination).await,
    }
}

/// Resolves on Ctrl-C. If the handler can't be installed, never resolves
/// (Ctrl-C then keeps its default behaviour).
async fn ctrl_c() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// After a command is accepted, wait for the car's answer (unless told not to).
///
/// The command id is shown as soon as the command is accepted, and again if
/// waiting fails or is interrupted, so the user can always look the result
/// up instead of sending the command again.
async fn await_outcome(
    client: &Client,
    car: &Vehicle,
    id: &CommandId,
    request: &Request<'_>,
    check: &str,
    json: bool,
) -> Result<ExitCode> {
    let Request { wait, label, .. } = *request;
    if wait.no_wait {
        output::command_sent(label, id, check, json)?;
        return Ok(ExitCode::SUCCESS);
    }
    if json {
        // The final JSON comes only after the car answers; the id is needed
        // before that if the wait is killed, so announce it at once.
        output::command_sent_event(label, id);
    } else {
        eprint!(
            "{label}: sent (id {}), waiting for the car…",
            clean(id.as_str())
        );
        std::io::stderr().flush().ok();
    }
    let started = Instant::now();
    let timeout = Duration::from_secs(wait.timeout);
    let problem = tokio::select! {
        result = client.wait_for(car, id, timeout, Duration::from_secs(3)) => match result {
            Ok(status) => {
                if !json {
                    eprintln!();
                }
                output::command_done(label, id, status, started.elapsed(), check, json)?;
                return Ok(if status == CommandStatus::Succeeded {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                });
            }
            Err(e) => format!("could not get the result: {}", output::error_chain(&e)),
        },
        () = ctrl_c() => "interrupted while waiting for the car".to_owned(),
    };
    if !json {
        eprintln!();
    }
    output::command_unconfirmed(label, id, &problem, check, json)?;
    Ok(ExitCode::FAILURE)
}

async fn trips(
    client: &Client,
    car: &Vehicle,
    month: Option<&str>,
    day: Option<jiff::civil::Date>,
    trip: Option<usize>,
    route: bool,
    json: bool,
) -> Result<()> {
    let Some(day) = day else {
        let (year, month) = if let Some(m) = month {
            parse_month(m)?
        } else {
            let today = output::india_today();
            (today.year(), today.month())
        };
        let summary = client.trips_in_month(car, year, month).await?;
        return output::trip_month(year, month, &summary, json);
    };
    let listing = client.trips_on_day(car, day).await?;
    let Some(n) = trip else {
        return output::trip_day(&listing, json);
    };
    let chosen = n
        .checked_sub(1)
        .and_then(|i| listing.trips.get(i))
        .with_context(|| format!("{day} has {} trips; pick 1 to {0}", listing.trips.len()))?;
    let detail = client.trip_detail(car, chosen).await?;
    output::trip_detail(n, &detail, json, route)
}

fn parse_month(raw: &str) -> Result<(i16, i8)> {
    let date = jiff::civil::Date::strptime("%Y-%m-%d", format!("{raw}-01"))
        .map_err(|_| anyhow::anyhow!("invalid month {raw:?}; use YYYY-MM, e.g. 2026-11"))?;
    Ok((date.year(), date.month()))
}

fn credentials_from_env(env: &store::EnvFile) -> Result<Credentials> {
    let var = |name: &str| env.var(name);
    let username =
        var("HKC_USERNAME").context("set HKC_USERNAME (your Bluelink / Kia Connect email)")?;
    let password = var("HKC_PASSWORD").context("set HKC_PASSWORD")?;
    let credentials = Credentials::new(username, password);
    Ok(match var("HKC_PIN") {
        Some(pin) => credentials.with_pin(pin),
        None => credentials,
    })
}

/// Choose the car a command acts on.
///
/// With no selection, an account with several cars is an error rather than
/// a guess. A selection matches, in order: an exact id, a VIN (ignoring
/// case), then a name (ignoring case), which must be unique.
fn pick_vehicle(mut vehicles: Vec<Vehicle>, wanted: Option<&str>) -> Result<Vehicle> {
    if vehicles.is_empty() {
        bail!("no vehicles on this account");
    }
    let Some(wanted) = wanted.map(str::trim).filter(|w| !w.is_empty()) else {
        if vehicles.len() == 1 {
            return Ok(vehicles.remove(0));
        }
        bail!(
            "this account has {} cars; choose one with --vehicle (or HKC_VEHICLE) \
             set to its name, VIN or id:\n{}",
            vehicles.len(),
            list(vehicles.iter())
        );
    };
    let index = if let Some(i) = vehicles.iter().position(|v| v.id == wanted) {
        i
    } else if let Some(i) = vehicles
        .iter()
        .position(|v| v.vin.eq_ignore_ascii_case(wanted))
    {
        i
    } else {
        let wanted_name = wanted.to_lowercase();
        let named: Vec<usize> = (0..vehicles.len())
            .filter(|&i| vehicles[i].name.trim().to_lowercase() == wanted_name)
            .collect();
        match named.as_slice() {
            [i] => *i,
            [] => bail!(
                "no car matches {wanted:?}; the account has:\n{}",
                list(vehicles.iter())
            ),
            _ => bail!(
                "{} cars are named {wanted:?}; choose one by VIN or id instead:\n{}",
                named.len(),
                list(named.iter().map(|&i| &vehicles[i]))
            ),
        }
    };
    Ok(vehicles.swap_remove(index))
}

fn list<'a>(vehicles: impl Iterator<Item = &'a Vehicle>) -> String {
    vehicles
        .map(|v| {
            format!(
                "  {:?}  {}  VIN {}  id {}",
                v.name,
                clean(&v.model),
                clean(&v.vin),
                clean(&v.id)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn confirm(yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    let stdin = std::io::stdin();
    let interactive = stdin.is_terminal();
    ask(question, interactive, &mut stdin.lock())
}

/// Ask a yes/no question on stderr; anything but "y" or "yes" cancels.
fn ask(question: &str, interactive: bool, input: &mut impl BufRead) -> Result<()> {
    if !interactive {
        bail!("{question} Refusing without --yes when not run interactively.");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("cancelled")
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use clap::{CommandFactory as _, Parser as _};
    use hkconnect::Vehicle;

    use super::{ask, keep_outcome, pick_vehicle};

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }

    #[test]
    fn generates_completions_for_every_shell() {
        use clap::ValueEnum as _;
        for shell in clap_complete::Shell::value_variants() {
            let mut out = Vec::new();
            clap_complete::generate(*shell, &mut super::Cli::command(), "hkc", &mut out);
            let script = String::from_utf8(out).unwrap();
            assert!(script.contains("hkc"), "{shell}: no command name");
            assert!(script.contains("status"), "{shell}: no subcommands");
        }
    }

    #[test]
    fn renders_the_man_page() {
        let mut out = Vec::new();
        clap_mangen::Man::new(super::Cli::command())
            .render(&mut out)
            .unwrap();
        let page = String::from_utf8(out).unwrap();
        assert!(page.starts_with(".ie") || page.contains(".TH"));
        assert!(page.contains("hkc"));
    }

    #[test]
    fn parses_months() {
        assert_eq!(super::parse_month("2026-11").unwrap(), (2026, 11));
        assert!(super::parse_month("2026-13").is_err());
        assert!(super::parse_month("nov").is_err());
    }

    /// Vehicles can't be built field by field outside the library; they
    /// deserialize from the library's own serialized form.
    fn car(id: &str, vin: &str, name: &str) -> Vehicle {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "vin": vin,
            "name": name,
            "model": "Creta",
            "year": null,
            "powertrain": "petrol",
            "ccs2": false,
        }))
        .unwrap()
    }

    fn garage() -> Vec<Vehicle> {
        vec![
            car("id-1", "VIN0000000000001", "Daily"),
            car("id-2", "VIN0000000000002", "Weekend"),
            car("id-3", "VIN0000000000003", "weekend"),
            // A name that looks like another car's id or VIN.
            car("id-4", "VIN0000000000004", "id-1"),
            car("id-5", "VIN0000000000005", "vin0000000000002"),
        ]
    }

    fn picked(wanted: Option<&str>) -> String {
        pick_vehicle(garage(), wanted).unwrap().id
    }

    fn error(vehicles: Vec<Vehicle>, wanted: Option<&str>) -> String {
        format!("{:#}", pick_vehicle(vehicles, wanted).unwrap_err())
    }

    #[test]
    fn single_car_is_picked_without_a_selection() {
        let only = vec![car("id-1", "VIN1", "Daily")];
        assert_eq!(pick_vehicle(only.clone(), None).unwrap().id, "id-1");
        assert_eq!(pick_vehicle(only, Some("  ")).unwrap().id, "id-1");
    }

    #[test]
    fn several_cars_need_a_selection() {
        let message = error(garage(), None);
        assert!(message.contains("--vehicle"), "{message}");
        assert!(message.contains("HKC_VEHICLE"), "{message}");
        assert!(message.contains("VIN0000000000005"), "{message}");
    }

    #[test]
    fn no_cars_is_an_error() {
        assert!(error(Vec::new(), None).contains("no vehicles"));
        assert!(error(Vec::new(), Some("Daily")).contains("no vehicles"));
    }

    #[test]
    fn id_wins_over_name() {
        assert_eq!(picked(Some("id-1")), "id-1");
    }

    #[test]
    fn vin_ignores_case_and_wins_over_name() {
        assert_eq!(picked(Some("vin0000000000002")), "id-2");
        assert_eq!(picked(Some("VIN0000000000003")), "id-3");
    }

    #[test]
    fn name_ignores_case() {
        assert_eq!(picked(Some("daily")), "id-1");
        assert_eq!(picked(Some(" DAILY ")), "id-1");
    }

    #[test]
    fn ambiguous_name_is_an_error() {
        let message = error(garage(), Some("WEEKEND"));
        assert!(message.contains("2 cars are named"), "{message}");
        assert!(
            message.contains("id-2") && message.contains("id-3"),
            "{message}"
        );
        assert!(!message.contains("id-1 "), "{message}");
    }

    #[test]
    fn unknown_selection_is_an_error() {
        let message = error(garage(), Some("Holiday"));
        assert!(message.contains("no car matches"), "{message}");
    }

    #[test]
    fn confirmation_refuses_without_a_terminal() {
        let message = format!(
            "{:#}",
            ask("Unlock?", false, &mut Cursor::new("y\n")).unwrap_err()
        );
        assert!(message.contains("--yes"), "{message}");
    }

    #[test]
    fn confirmation_needs_an_explicit_yes() {
        assert!(ask("Unlock?", true, &mut Cursor::new("y\n")).is_ok());
        assert!(ask("Unlock?", true, &mut Cursor::new("YES\n")).is_ok());
        assert!(ask("Unlock?", true, &mut Cursor::new("n\n")).is_err());
        assert!(ask("Unlock?", true, &mut Cursor::new("\n")).is_err());
        assert!(ask("Unlock?", true, &mut Cursor::new("")).is_err());
    }

    #[test]
    fn a_failed_session_save_never_changes_the_outcome() {
        use std::process::ExitCode;
        let dir = tempfile::tempdir().unwrap();
        // A data directory that cannot be created: its parent is a file.
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "").unwrap();
        let store = super::store::Store::at(&blocker.join("data"));
        let session: hkconnect::Session = serde_json::from_value(serde_json::json!({
            "access_token": "a", "refresh_token": "r",
            "expires_at": "2030-01-01T00:00:00Z", "device_id": "d",
        }))
        .unwrap();
        let save = || store.save_session(hkconnect::Brand::Hyundai, "me@example.com", &session);
        assert!(save().is_err());

        let kept = keep_outcome(Ok(ExitCode::SUCCESS), save()).unwrap();
        assert_eq!(kept, ExitCode::SUCCESS);
        let kept = keep_outcome(Ok(ExitCode::FAILURE), save()).unwrap();
        assert_eq!(kept, ExitCode::FAILURE);
        let err = keep_outcome(Err(anyhow::anyhow!("Unlock: refused")), save()).unwrap_err();
        assert_eq!(err.to_string(), "Unlock: refused");
    }

    #[test]
    fn brand_comes_from_the_flag_or_defaults_later() {
        let cli = super::Cli::try_parse_from(["hkc", "status"]).unwrap();
        // Unset here (unless HKC_BRAND is exported): `run` falls back to
        // the .env file, then to Hyundai.
        if std::env::var_os("HKC_BRAND").is_none() {
            assert!(cli.brand.is_none());
        }
        let cli = super::Cli::try_parse_from(["hkc", "--brand", "KIA", "status"]).unwrap();
        assert_eq!(cli.brand, Some(hkconnect::Brand::Kia));
    }

    #[test]
    fn start_options_are_checked_before_confirmation() {
        // `--yes` is absent and stdin is never read: an invalid request fails
        // validation before any question is asked.
        let cli = super::Cli::try_parse_from(["hkc", "start", "--minutes", "11"]).unwrap();
        let only = car("id-1", "VIN1", "Daily");
        let message = format!("{:#}", super::prepare(&cli.command, &only).err().unwrap());
        assert!(message.contains("invalid duration"), "{message}");

        let cli = super::Cli::try_parse_from(["hkc", "start", "--temp", "40"]).unwrap();
        let message = format!("{:#}", super::prepare(&cli.command, &only).err().unwrap());
        assert!(message.contains("invalid temperature"), "{message}");
    }
}
