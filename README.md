# iDRAC Fan Controller

Script to manually manage fan speeds as long as temperatures remain below a threshold.

Polls for the current temperature. Depending on the highest temperature read, the fan speed is manually configured.

The purpose is to decrease noise and power consumption when the system is idle. If the system goes out of idle and heats 
up, iDRAC's fan controller is re-enabled. After the system goes back to idle, the fan speed is manually lowered.

## Usage

To use the script, set the below environment variables:

`DELAY` (default 10s): the delay in seconds the script will poll for the current temperature. 

Fan speed is configured by passing arguments to the script:

Example: `idrac-fan-controller 50:10 60:20`

Arguments are of the form: `<max temp>:<fan speed>`

For example, as long as the highest measured temperature is below 50, the fan speed is set to 10%.

If the temperature is between 50 and 60, the fan speed is set to 20%.

If the temperature is higher than any configured temperature, iDRAC's fan controller is re-enabled.
In this example, if the temperature is above 60.
