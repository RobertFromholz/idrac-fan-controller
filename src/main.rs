use std::collections::{HashMap, VecDeque};
use std::fmt::Formatter;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::{fmt, thread};

const ROLLING_AVERAGE_WINDOW: usize = 5;
const DEFAULT_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_RAMP_DOWN_THRESHOLD: usize = 3;

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
struct Temperature(u32);

impl fmt::Display for Temperature {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}˚C", self.0)
    }
}

#[derive(Debug, Copy, Clone, Ord, PartialOrd, PartialEq, Eq)]
struct FanSpeed(u8);

impl FanSpeed {
    pub fn new(percentage: u8) -> FanSpeed {
        if percentage > 100 {
            panic!("percentage must be between 0 and 100");
        }
        FanSpeed(percentage)
    }
}

impl fmt::LowerHex for FanSpeed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for FanSpeed {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}%", self.0)
    }
}

#[derive(Debug)]
struct FanCurve {
    config: Vec<(Temperature, FanSpeed)>,
}

impl FanCurve {
    pub fn new(config: impl Into<Vec<(Temperature, FanSpeed)>>) -> FanCurve {
        let mut config = config.into();
        config.sort_by_key(|&(max_temp, _)| max_temp);
        FanCurve { config }
    }

    pub fn from_iter(iter: impl IntoIterator<Item=String>) -> FanCurve {
        let config = iter
            .into_iter()
            .map(|value| {
                let parts = value.split(":").collect::<Vec<_>>();
                assert_eq!(parts.len(), 2);
                let max_temp = parts[0]
                    .parse::<u32>()
                    .expect("couldn't parse max temperature");
                let fan_speed = parts[1].parse::<u8>().expect("couldn't parse fan speed");
                (Temperature(max_temp), FanSpeed::new(fan_speed))
            })
            .collect::<Vec<_>>();
        FanCurve::new(config)
    }

    /// The configured fan speed for the current temperature.
    /// Returns `None` if manual fan control should be disabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use idrac-fan-controller::FanConfig;
    ///
    /// let config = FanConfig::new(&[
    ///     (Temperature(50), FanSpeed::new(10)),
    ///     (Temperature(60), FanSpeed::new(20))
    /// ]);
    ///
    /// assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
    /// assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
    /// assert_eq!(config.fan_speed(Temperature(65)), None));
    /// ```
    pub fn fan_speed(&self, temperature: Temperature) -> Option<FanSpeed> {
        for &(max_temp, fan_speed) in &self.config {
            if temperature < max_temp {
                return Some(fan_speed);
            }
        }
        None
    }

    /// The configured fan speed for the current temperature, given a hysteresis factor.
    ///
    /// To decrease the fan speed, the temperature must be at least `hysteresis_offset` within the
    /// range for that fan speed. For example, to drop to `50:10` from `60:20`, the temperature
    /// must be at or below `47`, given an offset of `3` (the default).
    pub fn gradual_fan_speed(
        &self,
        temperature: Temperature,
        fan_speed: Option<FanSpeed>,
        hysteresis_offset: usize,
    ) -> Option<FanSpeed> {
        let target = self.fan_speed(temperature);

        // Check if we want to decrease the fan speed.
        match (target, fan_speed) {
            (Some(target), Some(current)) if target < current => {}
            _ => return target,
        };

        for &(max_temp, fan_speed) in &self.config {
            let offset_temp = Temperature(max_temp.0.saturating_sub(hysteresis_offset as u32));
            if temperature < offset_temp {
                return Some(fan_speed);
            }
        }

        fan_speed
    }
}

fn main() {
    // Load environment variables and the 'fan-curve'.
    let delay = std::env::var("DELAY")
        .ok()
        .map(|value| value.parse::<u64>().expect("couldn't parse DELAY"))
        .map(|delay| Duration::from_secs(delay))
        .unwrap_or(DEFAULT_DELAY);

    let ramp_down_threshold = std::env::var("RAMP_DOWN_THRESHOLD")
        .ok()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("couldn't parse RAMP_DOWN_THRESHOLD")
        })
        .unwrap_or(DEFAULT_RAMP_DOWN_THRESHOLD);

    let config = FanCurve::from_iter(std::env::args().skip(1));

    let mut idrac = Idrac::new();

    if let Some(metrics_address) = std::env::var("METRICS_ADDRESS").ok() {
        start_exporter(&idrac, metrics_address);
    }

    let mut rolling_window = VecDeque::with_capacity(ROLLING_AVERAGE_WINDOW);

    loop {
        // The program loop.
        // 1) Retrieve the system temperature.
        // 2) Configure the system's fan speed as desired.
        // 3) Wait
        // 4) Repeat
        match get_max_temperature() {
            Ok(last_temperature) => {
                // Keep a rolling average of the last N temperatures.
                if rolling_window.len() == ROLLING_AVERAGE_WINDOW {
                    rolling_window.pop_front();
                }
                rolling_window.push_back(last_temperature);
                // Calculate the rolling average.
                let temperature = rolling_window.iter().sum::<u32>() / (rolling_window.len() as u32);

                let current_fan_speed = idrac.fan_speed().lock().unwrap().clone();

                let fan_speed = config.gradual_fan_speed(
                    Temperature(temperature),
                    current_fan_speed,
                    ramp_down_threshold,
                );
                match fan_speed {
                    None => {
                        if let Err(msg) = idrac.disable_manual_fan_control() {
                            eprintln!("{}", msg);
                        }
                    }
                    Some(fan_speed) => {
                        if let Err(msg) = idrac.set_manual_fan_speed(fan_speed) {
                            eprintln!("{}", msg);
                        }
                    }
                }
            }
            Err(msg) => {
                eprintln!("{}", msg);
                let _ = idrac.disable_manual_fan_control();
            }
        }
        thread::sleep(delay);
    }
}

/// An object used to keep track of iDRAC's current fan speed, if any.
///
/// It also guarantees we at least try to re-active iDRAC's own fan-controller if the program
/// stops. It does this by implementing the `Drop` trait, which is called when the object goes out
/// of scope: if we either panic or exit.
struct Idrac {
    /// The current fan speed, if enabled.
    /// `None` if manual fan control is disabled.
    fan_speed: Arc<Mutex<Option<FanSpeed>>>,
}

impl Idrac {
    pub fn new() -> Idrac {
        Idrac {
            fan_speed: Arc::new(Mutex::new(None)),
        }
    }

    pub fn fan_speed(&self) -> Arc<Mutex<Option<FanSpeed>>> {
        self.fan_speed.clone()
    }

    fn enable_manual_fan_control() -> Result<(), String> {
        println!("Enabling manual fan control");
        raw_ipmitool("0x30 0x30 0x01 0x00")
            .map_err(|msg| format!("couldn't enable manual fan control: {msg}"))
    }

    pub fn disable_manual_fan_control(&mut self) -> Result<(), String> {
        println!("Disabling manual fan control");
        raw_ipmitool("0x30 0x30 0x01 0x01")
            .map_err(|msg| format!("couldn't disable manual fan control: {msg}"))?;
        let mut fan_speed = self.fan_speed.lock().unwrap();
        *fan_speed = None;
        Ok(())
    }

    pub fn set_manual_fan_speed(&mut self, percentage: FanSpeed) -> Result<(), String> {
        let fan_speed = self.fan_speed.lock().unwrap().clone();
        if fan_speed == Some(percentage) {
            return Ok(());
        }
        if fan_speed.is_none() {
            Idrac::enable_manual_fan_control()?;
        }
        println!("Setting manual fan speed to {}", percentage);
        raw_ipmitool(format!("0x30 0x30 0x02 0xff {:#04x}", percentage))
            .map_err(|msg| format!("couldn't set manual fan speed: {msg}"))?;
        let mut fan_speed = self.fan_speed.lock().unwrap();
        *fan_speed = Some(percentage);
        Ok(())
    }
}

impl Drop for Idrac {
    fn drop(&mut self) {
        let _ = self.disable_manual_fan_control();
    }
}

fn raw_ipmitool(arguments: impl Into<String>) -> Result<(), String> {
    // TODO: Let us configure our own login interface/arguments.
    //  Right now, we hard-code the 'open' interface. In the future, we might want
    //  to be able to control a remote iDRAC server.
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .arg("raw")
        .args(arguments.into().split_whitespace())
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(message.into())
    }
}

fn start_exporter(idrac: &Idrac, metrics_address: String) {
    let fan_speed = idrac.fan_speed();

    let listener = TcpListener::bind(&metrics_address).expect("couldn't start exporter");

    thread::spawn(move || {
        println!("Listening at: http://{metrics_address}/metrics");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_exporter_request(stream, &fan_speed),
                Err(e) => eprintln!("error accepting connection: {e}"),
            }
        }
    });
}

fn handle_exporter_request(mut stream: TcpStream, fan_speed: &Arc<Mutex<Option<FanSpeed>>>) {
    let mut buffer = String::new();

    match stream.read_to_string(&mut buffer) {
        Ok(_) => (),
        Err(e) => {
            eprintln!("error processing request: {e}");
            return;
        }
    };

    let path = buffer
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1));

    match path {
        Some("/metrics") => {
            let fan_speed = *fan_speed.lock().unwrap();
            let body = render_metrics(fan_speed);
            write_http_response(stream, "200 OK", "text/plain; version=0.0.4", &body);
        }
        _ => write_http_response(stream, "404 Not Found", "text/plain", "Not Found\n"),
    }
}

fn write_http_response(mut stream: TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\n\
        Content-Type: {content_type}\r\n\
        Content-Length: {}\r\n\
        Connection: close\r\n\
        \r\n\
        {body}",
        body.len()
    );
    if let Err(e) = stream.write_all(response.as_bytes()) {
        eprintln!("error writing response: {e}");
    }
}

fn render_metrics(fan_speed: Option<FanSpeed>) -> String {
    let speed = fan_speed
        // We can convert a u8 to an i8 since the value will only be between 0 and 100.
        .map(|fan_speed| fan_speed.0 as i8)
        .unwrap_or(-1);

    let status: u8 = match fan_speed {
        None => 0,
        Some(_) => 1
    };

    format!(
        "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
         # TYPE idrac_fan_controller_speed gauge\n\
         idrac_fan_controller_speed {speed}\n\
         # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
         # TYPE idrac_fan_controller_status gauge\n\
         idrac_fan_controller_status {status}\n"
    )
}

fn get_max_temperature() -> Result<u32, String> {
    let temperatures = get_temperatures()?;
    temperatures
        .into_values()
        .max()
        .ok_or_else(|| "couldn't find temperature".to_owned())
}

fn get_temperatures() -> Result<HashMap<String, u32>, String> {
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .args(&["sdr", "type", "temperature"])
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    if output.status.success() {
        let output = String::from_utf8(output.stdout)
            .map_err(|e| format!("couldn't parse ipmitool output: {}", e))?;
        let mut temperatures = HashMap::new();
        for line in output.lines() {
            let parts = line.split("|").collect::<Vec<_>>();
            if parts.len() != 5 {
                continue;
            }
            let name = parts[0].trim();
            let temperature = parts[4].trim();
            if temperature == "No Reading" {
                continue;
            }
            let temperature = temperature
                .split(" ")
                .next()
                .and_then(|value| value.parse::<u32>().ok());
            if let Some(temperature) = temperature {
                temperatures.insert(name.to_owned(), temperature);
            };
        }
        Ok(temperatures)
    } else {
        let message = String::from_utf8_lossy(&output.stderr);
        Err(message.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fan_config_from_empty_iter() {
        let config = FanCurve::from_iter(Vec::<String>::new());
        assert_eq!(Vec::<(Temperature, FanSpeed)>::new(), config.config)
    }

    #[test]
    fn test_fan_config_from_iter() {
        let config = FanCurve::from_iter(["50:10".to_owned(), "60:20".to_owned()]);
        assert_eq!(
            vec![
                (Temperature(50), FanSpeed::new(10)),
                (Temperature(60), FanSpeed::new(20))
            ],
            config.config
        )
    }

    #[test]
    fn test_fan_config_from_unsorted_iter() {
        let config = FanCurve::from_iter(["60:20".to_owned(), "50:10".to_owned()]);
        assert_eq!(
            vec![
                (Temperature(50), FanSpeed::new(10)),
                (Temperature(60), FanSpeed::new(20))
            ],
            config.config
        )
    }

    #[test]
    fn test_empty_fan_config() {
        let config = FanCurve::new(&[]);
        assert_eq!(config.fan_speed(Temperature(0)), None);
        assert_eq!(config.fan_speed(Temperature(100)), None);
    }

    #[test]
    fn test_simple_fan_config() {
        let config = FanCurve::new(&[(Temperature(50), FanSpeed::new(10))]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), None);
        assert_eq!(config.fan_speed(Temperature(55)), None);
    }

    #[test]
    fn test_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(60)), None);
        assert_eq!(config.fan_speed(Temperature(65)), None);
    }

    #[test]
    fn test_unsorted_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(60), FanSpeed::new(20)),
            (Temperature(50), FanSpeed::new(10)),
        ]);
        assert_eq!(config.fan_speed(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.fan_speed(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.fan_speed(Temperature(60)), None);
        assert_eq!(config.fan_speed(Temperature(65)), None);
    }

    #[test]
    fn test_hysteresis_ramp_up_instant() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);

        // The fan speed should increase immediately.
        let target = config.gradual_fan_speed(Temperature(50), Some(FanSpeed::new(10)), 3);
        assert_eq!(target, Some(FanSpeed::new(20)));
    }

    #[test]
    fn test_hysteresis_ramp_down_delayed() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);

        // The fan speed should not decrease, since the new temperature is not 3 below the
        // specified temperature (50 - 3 = 47).
        let target = config.gradual_fan_speed(Temperature(48), Some(FanSpeed::new(20)), 3);
        assert_eq!(target, Some(FanSpeed::new(20)));

        // The temperature drops to 46 (4 below), the fan speed should now decrease.
        let target_cooled = config.gradual_fan_speed(Temperature(46), Some(FanSpeed::new(20)), 3);
        assert_eq!(target_cooled, Some(FanSpeed::new(10)));
    }

    #[test]
    fn test_render_metrics_when_manual_fan_control_is_disabled() {
        assert_eq!(
            render_metrics(None),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed -1\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 0\n"
        );
    }

    #[test]
    fn test_render_metrics_when_manual_fan_control_is_enabled() {
        assert_eq!(
            render_metrics(Some(FanSpeed::new(20))),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed 20\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 1\n"
        );
    }
}
