use std::collections::{HashMap, VecDeque};
use std::fmt::Formatter;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, thread};
use std::iter::Sum;

const ROLLING_AVERAGE_WINDOW: usize = 5;
const DEFAULT_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_RAMP_DOWN_THRESHOLD: usize = 3;
const DEFAULT_RAMP_UP_DELAY: Duration = Duration::from_secs(10);
const DEFAULT_CRITICAL_TEMPERATURE: Temperature = Temperature(90);

#[derive(Debug, Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
struct Temperature(u32);

impl<'a> Sum<&'a Temperature> for u32 {
    fn sum<I: Iterator<Item=&'a Temperature>>(iter: I) -> Self {
        iter.map(|t| t.0).sum()
    }
}

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
    /// How far below a fan speed's max temperature the temperature must drop before decreasing
    /// to that fan speed.
    ramp_down_threshold: usize,
    /// How long the temperature must call for a higher fan speed before increasing the fan speed.
    ramp_up_delay: Duration,
    /// The temperature at which iDRAC's fan controller is re-activated immediately.
    critical_temperature: Temperature,
    /// The last `ROLLING_AVERAGE_WINDOW` temperature readings.
    rolling_window: VecDeque<Temperature>,
    /// When the latest temperature reading started calling for a higher fan speed, if it still does.
    elevated_since: Option<Instant>,
}

impl FanCurve {
    pub fn new(config: impl Into<Vec<(Temperature, FanSpeed)>>) -> FanCurve {
        let mut config = config.into();
        config.sort_by_key(|&(max_temp, _)| max_temp);
        FanCurve {
            config,
            ramp_down_threshold: DEFAULT_RAMP_DOWN_THRESHOLD,
            ramp_up_delay: DEFAULT_RAMP_UP_DELAY,
            critical_temperature: DEFAULT_CRITICAL_TEMPERATURE,
            rolling_window: VecDeque::with_capacity(ROLLING_AVERAGE_WINDOW),
            elevated_since: None,
        }
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

    pub fn with_ramp_down_threshold(mut self, ramp_down_threshold: usize) -> FanCurve {
        self.ramp_down_threshold = ramp_down_threshold;
        self
    }

    pub fn with_ramp_up_delay(mut self, ramp_up_delay: Duration) -> FanCurve {
        self.ramp_up_delay = ramp_up_delay;
        self
    }

    pub fn with_critical_temperature(mut self, critical_temperature: Temperature) -> FanCurve {
        self.critical_temperature = critical_temperature;
        self
    }

    /// The fan speed to apply, given the latest temperature reading and the current fan speed.
    /// Returns `None` if manual fan control should be disabled.
    ///
    /// The fan speed is determined by a rolling average of the last `ROLLING_AVERAGE_WINDOW`
    /// temperature readings (see `gradual_fan_speed`).
    ///
    /// Increasing the fan speed is delayed until the latest temperature reading has called for a
    /// higher fan speed for at least `ramp_up_delay`. This stops short temperature spikes from
    /// ramping up the fans. The rolling average can't do this on its own: a single spike raises
    /// the average for the whole rolling window. Decreasing the fan speed and re-activating
    /// iDRAC's fan controller is never delayed.
    ///
    /// If the latest temperature reading is at or above `critical_temperature`, iDRAC's fan
    /// controller is always re-activated, regardless of the rolling average.
    pub fn fan_speed(
        &mut self,
        temperature: Temperature,
        current: Option<FanSpeed>,
        now: Instant,
    ) -> Option<FanSpeed> {
        // Keep a rolling average of the last N temperatures.
        if self.rolling_window.len() == ROLLING_AVERAGE_WINDOW {
            self.rolling_window.pop_front();
        }
        self.rolling_window.push_back(temperature);

        // Re-activate iDRAC's fan controller immediately if something has really spiked.
        if temperature >= self.critical_temperature {
            self.elevated_since = None;
            return None;
        }

        let average = self.rolling_window.iter().sum::<u32>() / (self.rolling_window.len() as u32);

        // Get the target temperature, based solely on the current temperature, given a 'RAMP_DOWN_THRESHOLD'.
        let target = self.gradual_fan_speed(Temperature(average), current);

        let Some(current) = current else {
            self.elevated_since = None;
            return target;
        };

        // `None` means iDRAC's fan controller, which counts as a higher fan speed.
        if self.lookup(temperature).is_none_or(|latest| latest > current) {
            self.elevated_since.get_or_insert(now);
        } else {
            self.elevated_since = None;
        }

        match target {
            Some(target) if target > current => match self.elevated_since {
                // Only increase fan speed after a specified duration.
                Some(since) if now.duration_since(since) >= self.ramp_up_delay => {
                    self.elevated_since = None;
                    Some(target)
                }
                _ => Some(current),
            },
            // This means either:
            // - manual fan speed should be disabled.
            // - fan speed is kept at the same level.
            // - fan speed is lowered.
            // Regardless, we do this immediately.
            target => target,
        }
    }

    /// The configured fan speed for the temperature.
    /// Returns `None` if manual fan control should be disabled.
    ///
    /// # Examples
    ///
    /// ```
    /// use idrac-fan-controller::FanCurve;
    ///
    /// let config = FanCurve::new(&[
    ///     (Temperature(50), FanSpeed::new(10)),
    ///     (Temperature(60), FanSpeed::new(20))
    /// ]);
    ///
    /// assert_eq!(config.lookup(Temperature(45)), Some(FanSpeed::new(10)));
    /// assert_eq!(config.lookup(Temperature(55)), Some(FanSpeed::new(20)));
    /// assert_eq!(config.lookup(Temperature(65)), None));
    /// ```
    fn lookup(&self, temperature: Temperature) -> Option<FanSpeed> {
        for &(max_temp, fan_speed) in &self.config {
            if temperature < max_temp {
                return Some(fan_speed);
            }
        }
        None
    }

    /// The configured fan speed for the temperature, given the ramp down threshold.
    ///
    /// To decrease the fan speed, the temperature must be at least `ramp_down_threshold` within
    /// the range for that fan speed. For example, to drop to `50:10` from `60:20`, the temperature
    /// must be at or below `47`, given a threshold of `3` (the default).
    fn gradual_fan_speed(
        &self,
        temperature: Temperature,
        fan_speed: Option<FanSpeed>,
    ) -> Option<FanSpeed> {
        let target = self.lookup(temperature);

        // Check if we want to decrease the fan speed.
        match (target, fan_speed) {
            (Some(target), Some(current)) if target < current => {}
            _ => return target,
        };

        for &(max_temp, fan_speed) in &self.config {
            let offset_temp =
                Temperature(max_temp.0.saturating_sub(self.ramp_down_threshold as u32));
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

    let ramp_up_delay = std::env::var("RAMP_UP_DELAY")
        .ok()
        .map(|value| value.parse::<u64>().expect("couldn't parse RAMP_UP_DELAY"))
        .map(|delay| Duration::from_secs(delay))
        .unwrap_or(DEFAULT_RAMP_UP_DELAY);

    let critical_temperature = std::env::var("CRITICAL_TEMPERATURE")
        .ok()
        .map(|value| value.parse::<u32>().expect("couldn't parse CRITICAL_TEMPERATURE"))
        .map(|temperature| Temperature(temperature))
        .unwrap_or(DEFAULT_CRITICAL_TEMPERATURE);

    let mut config = FanCurve::from_iter(std::env::args().skip(1))
        .with_ramp_down_threshold(ramp_down_threshold)
        .with_ramp_up_delay(ramp_up_delay)
        .with_critical_temperature(critical_temperature);

    let mut idrac = Idrac::new();

    let temperatures = Arc::new(Mutex::new(HashMap::new()));
    let fan_speeds = Arc::new(Mutex::new(HashMap::new()));

    if let Some(metrics_address) = std::env::var("METRICS_ADDRESS").ok() {
        start_exporter(idrac.fan_speed(), temperatures.clone(), fan_speeds.clone(), metrics_address);
    }

    loop {
        {
            match get_fan_speeds() {
                Ok(current_fan_speeds) => {
                    let mut fan_speeds = fan_speeds.lock().unwrap();
                    *fan_speeds = current_fan_speeds;
                }
                Err(e) => {
                    eprintln!("{}", e);
                }
            };
        }
        match get_max_temperature(temperatures.clone()) {
            Ok(temperature) => {
                let current_fan_speed = idrac.fan_speed().lock().unwrap().clone();
                let fan_speed =
                    config.fan_speed(Temperature(temperature), current_fan_speed, Instant::now());
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

fn start_exporter(
    fan_speed: Arc<Mutex<Option<FanSpeed>>>,
    temperatures: Arc<Mutex<HashMap<(String, String), u32>>>,
    fan_speeds: Arc<Mutex<HashMap<(String, String), u32>>>,
    metrics_address: String,
) {
    let listener = TcpListener::bind(&metrics_address).expect("couldn't start exporter");

    thread::spawn(move || {
        println!("Listening at: http://{metrics_address}/metrics");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => handle_exporter_request(stream, &fan_speed, &temperatures, &fan_speeds),
                Err(e) => eprintln!("error accepting connection: {e}"),
            }
        }
    });
}

fn handle_exporter_request(
    stream: TcpStream,
    fan_speed: &Arc<Mutex<Option<FanSpeed>>>,
    temperatures: &Arc<Mutex<HashMap<(String, String), u32>>>,
    fan_speeds: &Arc<Mutex<HashMap<(String, String), u32>>>,
) {
    let path = match read_http_request(&stream) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("error processing request: {e}");
            return;
        }
    };

    match path.as_deref() {
        Some("/metrics") => {
            let fan_speed = *fan_speed.lock().unwrap();
            let temperatures = temperatures.lock().unwrap();
            let fan_speeds = fan_speeds.lock().unwrap();
            let body = render_metrics(fan_speed, &temperatures, &fan_speeds);
            write_http_response(stream, "200 OK", "text/plain; version=0.0.4", &body);
        }
        _ => write_http_response(stream, "404 Not Found", "text/plain", "Not Found\n"),
    }
}

fn read_http_request(stream: &TcpStream) -> std::io::Result<Option<String>> {
    let reader = BufReader::new(stream);
    let request = reader.lines().next().transpose()?;
    let Some(request) = request else {
        return Ok(None);
    };
    let path = request.split_whitespace().nth(1).map(str::to_owned);
    Ok(path)
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

fn render_metrics(
    fan_speed: Option<FanSpeed>,
    temperatures: &HashMap<(String, String), u32>,
    fan_speeds: &HashMap<(String, String), u32>,
) -> String {
    let speed = fan_speed
        // We can convert an u8 to an i8 since the value will only be between 0 and 100.
        .map(|fan_speed| fan_speed.0 as i8)
        .unwrap_or(-1);

    let status: u8 = match fan_speed {
        None => 0,
        Some(_) => 1,
    };

    format!(
        "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
         # TYPE idrac_fan_controller_speed gauge\n\
         idrac_fan_controller_speed {speed}\n\
         # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
         # TYPE idrac_fan_controller_status gauge\n\
         idrac_fan_controller_status {status}\n\
         # HELP idrac_temperature Temperature sensor reading in degrees Celsius.\n\
         # TYPE idrac_temperature gauge\n\
         {}\
         # HELP idrac_fan_speed Fan speed sensor reading in RPM.\n\
         # TYPE idrac_fan_speed gauge\n\
         {}\
         ",
        render_sensor_data("idrac_temperature", temperatures),
        render_sensor_data("idrac_fan_speed", fan_speeds),
    )
}

fn render_sensor_data(metric: &str, data: &HashMap<(String, String), u32>) -> String {
    let mut output = String::new();
    let mut temperatures = data.iter().collect::<Vec<_>>();
    temperatures.sort_by_key(|(name, _)| *name);

    for ((name, id), temperature) in temperatures {
        let (name, id) = (clean_name(name), clean_name(id));
        output += &format!("{metric}{{name=\"{name}\", id=\"{id}\"}} {temperature}\n");
    }
    output
}

fn clean_name(name: &str) -> String {
    name
        .replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('"', r#"\""#)
}

fn get_max_temperature(temperatures: Arc<Mutex<HashMap<(String, String), u32>>>) -> Result<u32, String> {
    let current_temperatures = get_temperatures()?;
    let mut temperatures = temperatures.lock().unwrap();
    *temperatures = current_temperatures;
    temperatures
        .values()
        .copied()
        .max()
        .ok_or_else(|| "couldn't find temperature".to_owned())
}

fn get_temperatures() -> Result<HashMap<(String, String), u32>, String> {
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .args(&["sdr", "type", "temperature"])
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    format_sensors(output)
}

fn get_fan_speeds() -> Result<HashMap<(String, String), u32>, String> {
    let output = Command::new("ipmitool")
        .arg("-I")
        .arg("open")
        .args(&["sdr", "type", "fan"])
        .output()
        .map_err(|msg| format!("couldn't invoke ipmitool: {msg}"))?;
    format_sensors(output)
}

fn format_sensors(output: Output) -> Result<HashMap<(String, String), u32>, String> {
    if output.status.success() {
        let output = String::from_utf8(output.stdout)
            .map_err(|e| format!("couldn't parse ipmitool output: {}", e))?;
        let mut sensors = HashMap::new();
        for line in output.lines() {
            let parts = line.split("|").collect::<Vec<_>>();
            if parts.len() != 5 {
                continue;
            }
            let name = parts[0].trim();
            let id = parts[1].trim();
            let temperature = parts[4].trim();
            if temperature == "No Reading" || temperature == "Disabled" {
                continue;
            }
            let temperature = temperature
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u32>().ok());
            if let Some(temperature) = temperature {
                sensors.insert((name.to_owned(), id.to_owned()), temperature);
            };
        }
        Ok(sensors)
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
        assert_eq!(config.lookup(Temperature(0)), None);
        assert_eq!(config.lookup(Temperature(100)), None);
    }

    #[test]
    fn test_simple_fan_config() {
        let config = FanCurve::new(&[(Temperature(50), FanSpeed::new(10))]);
        assert_eq!(config.lookup(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.lookup(Temperature(50)), None);
        assert_eq!(config.lookup(Temperature(55)), None);
    }

    #[test]
    fn test_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);
        assert_eq!(config.lookup(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.lookup(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.lookup(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.lookup(Temperature(60)), None);
        assert_eq!(config.lookup(Temperature(65)), None);
    }

    #[test]
    fn test_unsorted_fan_config() {
        let config = FanCurve::new(&[
            (Temperature(60), FanSpeed::new(20)),
            (Temperature(50), FanSpeed::new(10)),
        ]);
        assert_eq!(config.lookup(Temperature(45)), Some(FanSpeed::new(10)));
        assert_eq!(config.lookup(Temperature(50)), Some(FanSpeed::new(20)));
        assert_eq!(config.lookup(Temperature(55)), Some(FanSpeed::new(20)));
        assert_eq!(config.lookup(Temperature(60)), None);
        assert_eq!(config.lookup(Temperature(65)), None);
    }

    #[test]
    fn test_hysteresis_ramp_up_instant() {
        let config = FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
        ]);

        // The fan speed should increase immediately.
        let target = config.gradual_fan_speed(Temperature(50), Some(FanSpeed::new(10)));
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
        let target = config.gradual_fan_speed(Temperature(48), Some(FanSpeed::new(20)));
        assert_eq!(target, Some(FanSpeed::new(20)));

        // The temperature drops to 46 (4 below), the fan speed should now decrease.
        let target_cooled = config.gradual_fan_speed(Temperature(46), Some(FanSpeed::new(20)));
        assert_eq!(target_cooled, Some(FanSpeed::new(10)));
    }

    fn ramp_up_curve() -> FanCurve {
        FanCurve::new(&[
            (Temperature(50), FanSpeed::new(10)),
            (Temperature(60), FanSpeed::new(20)),
            (Temperature(70), FanSpeed::new(30)),
        ]).with_ramp_up_delay(Duration::from_secs(10))
    }

    #[test]
    fn test_ramp_up_delayed() {
        let mut config = ramp_up_curve();
        let (low, high) = (Some(FanSpeed::new(10)), Some(FanSpeed::new(20)));
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        assert_eq!(config.fan_speed(Temperature(55), low, at(0)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(5)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(10)), high);
    }

    #[test]
    fn test_ramp_up_delay_resets_when_temperature_drops() {
        let mut config = ramp_up_curve();
        let (low, high) = (Some(FanSpeed::new(10)), Some(FanSpeed::new(20)));
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        assert_eq!(config.fan_speed(Temperature(55), low, at(0)), low);
        // The spike is over, but the rolling average (50) still calls for a higher fan speed.
        assert_eq!(config.fan_speed(Temperature(45), low, at(5)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(10)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(15)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(20)), high);
    }

    #[test]
    fn test_ramp_up_delay_waits_for_average() {
        let mut config = ramp_up_curve();
        let (low, high) = (Some(FanSpeed::new(10)), Some(FanSpeed::new(20)));
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        for _ in 0..4 {
            assert_eq!(config.fan_speed(Temperature(40), low, at(0)), low);
        }
        // The latest temperature calls for a higher fan speed, but the rolling average doesn't
        // until it reaches 52.
        assert_eq!(config.fan_speed(Temperature(55), low, at(0)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(10)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(20)), low);
        assert_eq!(config.fan_speed(Temperature(55), low, at(30)), high);
    }

    #[test]
    fn test_ramp_up_delay_restarts_after_ramping_up() {
        let mut config = ramp_up_curve();
        let (low, mid, high) = (
            Some(FanSpeed::new(10)),
            Some(FanSpeed::new(20)),
            Some(FanSpeed::new(30)),
        );
        let start = Instant::now();
        let at = |secs| start + Duration::from_secs(secs);

        for _ in 0..4 {
            assert_eq!(config.fan_speed(Temperature(45), low, at(0)), low);
        }
        assert_eq!(config.fan_speed(Temperature(65), low, at(0)), low);
        assert_eq!(config.fan_speed(Temperature(65), low, at(10)), mid);
        assert_eq!(config.fan_speed(Temperature(65), mid, at(15)), mid);
        assert_eq!(config.fan_speed(Temperature(65), mid, at(25)), high);
    }

    #[test]
    fn test_ramp_up_without_delay() {
        let mut config = ramp_up_curve().with_ramp_up_delay(Duration::ZERO);
        let (low, high) = (Some(FanSpeed::new(10)), Some(FanSpeed::new(20)));

        assert_eq!(config.fan_speed(Temperature(55), low, Instant::now()), high);
    }

    #[test]
    fn test_ramp_down_not_delayed() {
        let mut config = ramp_up_curve();
        let (low, high) = (Some(FanSpeed::new(10)), Some(FanSpeed::new(20)));

        assert_eq!(config.fan_speed(Temperature(40), high, Instant::now()), low);
    }

    #[test]
    fn test_idrac_fan_control_not_delayed() {
        let mut config = ramp_up_curve();
        let low = Some(FanSpeed::new(10));

        assert_eq!(config.fan_speed(Temperature(75), low, Instant::now()), None);
    }

    #[test]
    fn test_manual_fan_control_not_delayed() {
        let mut config = ramp_up_curve();
        let low = Some(FanSpeed::new(10));

        assert_eq!(config.fan_speed(Temperature(45), None, Instant::now()), low);
    }

    #[test]
    fn test_critical_temperature_disables_manual_fan_control_immediately() {
        let mut config = ramp_up_curve().with_critical_temperature(Temperature(90));
        let low = Some(FanSpeed::new(10));
        let now = Instant::now();

        for _ in 0..4 {
            assert_eq!(config.fan_speed(Temperature(40), low, now), low);
        }
        // The rolling average (50) is still well within the fan curve.
        assert_eq!(config.fan_speed(Temperature(90), low, now), None);
    }

    #[test]
    fn test_below_critical_temperature_uses_rolling_average() {
        let mut config = ramp_up_curve().with_critical_temperature(Temperature(90));
        let low = Some(FanSpeed::new(10));
        let now = Instant::now();

        for _ in 0..4 {
            assert_eq!(config.fan_speed(Temperature(40), low, now), low);
        }
        assert_eq!(config.fan_speed(Temperature(89), low, now), low);
    }

    #[test]
    fn test_render_metrics_when_manual_fan_control_is_disabled() {
        assert_eq!(
            render_metrics(None, &HashMap::new(), &HashMap::new()),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed -1\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 0\n\
             # HELP idrac_temperature Temperature sensor reading in degrees Celsius.\n\
             # TYPE idrac_temperature gauge\n\
             # HELP idrac_fan_speed Fan speed sensor reading in RPM.\n\
             # TYPE idrac_fan_speed gauge\n"
        );
    }

    #[test]
    fn test_render_metrics_when_manual_fan_control_is_enabled() {
        assert_eq!(
            render_metrics(Some(FanSpeed::new(20)), &HashMap::new(), &HashMap::new()),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed 20\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 1\n\
             # HELP idrac_temperature Temperature sensor reading in degrees Celsius.\n\
             # TYPE idrac_temperature gauge\n\
             # HELP idrac_fan_speed Fan speed sensor reading in RPM.\n\
             # TYPE idrac_fan_speed gauge\n"
        );
    }

    #[test]
    fn test_render_metrics_includes_temperatures() {
        let temperatures = HashMap::from([
            (("Exhaust Temp".to_owned(), "0x00".to_owned()), 40),
            (("Ambient Temp".to_owned(), "0x01".to_owned()), 21),
        ]);

        assert_eq!(
            render_metrics(Some(FanSpeed::new(20)), &temperatures, &HashMap::new()),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed 20\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 1\n\
             # HELP idrac_temperature Temperature sensor reading in degrees Celsius.\n\
             # TYPE idrac_temperature gauge\n\
             idrac_temperature{name=\"Ambient Temp\", id=\"0x01\"} 21\n\
             idrac_temperature{name=\"Exhaust Temp\", id=\"0x00\"} 40\n\
             # HELP idrac_fan_speed Fan speed sensor reading in RPM.\n\
             # TYPE idrac_fan_speed gauge\n"
        );
    }

    #[test]
    fn test_render_metrics_escapes_temperature_names() {
        let temperatures = HashMap::from([
            (("Sensor \"A\" \\ B".to_owned(), "0x \\ 0 \"".to_owned()), 40)
        ]);

        assert_eq!(
            render_metrics(None, &temperatures, &HashMap::new()),
            "# HELP idrac_fan_controller_speed Fan speed as a percent from 0 or 100, or -1 if manual fan speed is disabled.\n\
             # TYPE idrac_fan_controller_speed gauge\n\
             idrac_fan_controller_speed -1\n\
             # HELP idrac_fan_controller_status Is manual fan speed enabled (1 = yes, 0 = no).\n\
             # TYPE idrac_fan_controller_status gauge\n\
             idrac_fan_controller_status 0\n\
             # HELP idrac_temperature Temperature sensor reading in degrees Celsius.\n\
             # TYPE idrac_temperature gauge\n\
             idrac_temperature{name=\"Sensor \\\"A\\\" \\\\ B\", id=\"0x \\\\ 0 \\\"\"} 40\n\
             # HELP idrac_fan_speed Fan speed sensor reading in RPM.\n\
             # TYPE idrac_fan_speed gauge\n"
        );
    }
}
