# TCP over WebSocket Rust (tcp2ws-rs)

Inspired by the original [tcp-over-websocket](https://github.com/zanjie1999/tcp-over-websocket)

This is a Rust implementation of TCP and UDP over WebSocket, allowing you to tunnel TCP and UDP traffic through WebSocket connections. It can be used for various purposes, such as bypassing firewalls or proxy servers.

Base code from Gemini 2.5 Pro, Optimized by Claude 4 Sonnet.

## Features
- [x] TCP over WebSocket
- [x] UDP over WebSocket


## Usage
#### Server
```bash
tcp-over-ws-rust server 0.0.0.0:8080 127.0.0.1:5201
```
#### Client
```bash
tcp-over-ws-rust client ws://your-server.com:8080 127.0.0.1:5200
```

## Performance Benchmarks

Performance tests conducted using iPerf3 to measure throughput efficiency.

### Test Environment
- **CPU**: Intel(R) Xeon(R) CPU E5-2680 v3 @ 2.50GHz  
- **RAM**: 32GB  
- **OS**: Windows 11 Pro   
- **iPerf3**: Version 3.19 (cJSON 1.7.15)  

### Benchmark Setup
- **Server**: `iperf3 -s`  
- **Client**: `iperf3 -c localhost -p $PORT`

#### Direct 
```text
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.00   sec   954 MBytes  7.98 Gbits/sec
[  5]   1.00-2.01   sec   971 MBytes  8.09 Gbits/sec
[  5]   2.01-3.00   sec   964 MBytes  8.13 Gbits/sec
[  5]   3.00-4.01   sec   994 MBytes  8.26 Gbits/sec
[  5]   4.01-5.01   sec   972 MBytes  8.20 Gbits/sec
[  5]   5.01-6.00   sec   964 MBytes  8.16 Gbits/sec
[  5]   6.00-7.01   sec   986 MBytes  8.21 Gbits/sec
[  5]   7.01-8.00   sec   966 MBytes  8.15 Gbits/sec
[  5]   8.00-9.00   sec   969 MBytes  8.14 Gbits/sec
[  5]   9.00-10.00  sec   972 MBytes  8.14 Gbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-10.00  sec  9.48 GBytes  8.15 Gbits/sec                  sender
[  5]   0.00-10.00  sec  9.48 GBytes  8.15 Gbits/sec                  receiver
```

#### WebSocket
```text
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-1.01   sec   327 MBytes  2.72 Gbits/sec
[  5]   1.01-2.00   sec   322 MBytes  2.72 Gbits/sec
[  5]   2.00-3.01   sec   333 MBytes  2.75 Gbits/sec
[  5]   3.01-4.01   sec   331 MBytes  2.79 Gbits/sec
[  5]   4.01-5.00   sec   328 MBytes  2.76 Gbits/sec
[  5]   5.00-6.00   sec   330 MBytes  2.78 Gbits/sec
[  5]   6.00-7.00   sec   329 MBytes  2.76 Gbits/sec
[  5]   7.00-8.00   sec   337 MBytes  2.83 Gbits/sec
[  5]   8.00-9.01   sec   333 MBytes  2.77 Gbits/sec
[  5]   9.01-10.01  sec   335 MBytes  2.79 Gbits/sec
- - - - - - - - - - - - - - - - - - - - - - - - -
[ ID] Interval           Transfer     Bitrate
[  5]   0.00-10.01  sec  3.23 GBytes  2.77 Gbits/sec                  sender
[  5]   0.00-10.02  sec  3.22 GBytes  2.76 Gbits/sec                  receiver
```