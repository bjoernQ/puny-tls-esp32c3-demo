# Using puny-tls-rs on ESP32-C3

This connects to tls13.akamai.io:443 and does a GET request on the index page.

This uses [puny-tls](https://github.com/bjoernQ/puny-tls) to handle the TLS connect.

In order to build this you need to set `SSID` and `PASSWORD` environment variables AND you need to do a release build!
