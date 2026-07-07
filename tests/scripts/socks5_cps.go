// socks5_cps.go -- a tiny, dependency-free (stdlib-only) SOCKS5 load client for
// next-socks5 benchmarking.
//
// Client modes (-mode):
//
//   cps (default): for -d duration with -c concurrent workers, each worker
//     repeatedly opens a TCP connection to the proxy, performs a full RFC 1928
//     handshake (+ RFC 1929 auth if -user is set) and a CONNECT to -target, then
//     closes immediately. Reports connection-establishment rate (CPS, RFC 3511
//     5.3) and handshake-latency percentiles. This is the accurate CPS tool that
//     curl/proxychains cannot be (no per-request process spawn).
//
//   thr: each of the -c workers opens ONE connection and reads bulk data from
//     the target (run the sink with -blast) until the deadline. Reports
//     aggregate and per-stream relay throughput. Use to sweep relay buffer
//     sizes / stream counts.
//
//   hold: ramp up -c connections (handshake + CONNECT each) and HOLD them all
//     open and idle until the deadline (RFC 9411 7.5 concurrent-capacity
//     style). Reports achieved concurrency; watch the proxy's RSS/fds
//     externally to get per-connection cost.
//
//   sink (-sink ADDR): a lightweight TCP server used as the CONNECT target so
//     the upstream never bottlenecks: accept-and-drain by default, or a zero
//     blaster with -blast (for -mode thr).
package main

import (
	"flag"
	"fmt"
	"io"
	"net"
	"sort"
	"sync"
	"sync/atomic"
	"time"
)

var (
	proxy  = flag.String("proxy", "127.0.0.1:11080", "SOCKS5 proxy host:port")
	user   = flag.String("user", "", "username (empty = no auth)")
	pass   = flag.String("pass", "", "password")
	target = flag.String("target", "127.0.0.1:19090", "CONNECT target (IPv4 literal host:port)")
	conc   = flag.Int("c", 300, "concurrent workers")
	dur    = flag.Duration("d", 20*time.Second, "test duration")
	mode   = flag.String("mode", "cps", "client mode: cps | thr | hold")
	sink   = flag.String("sink", "", "run as a TCP sink on this addr instead of a client")
	blast  = flag.Bool("blast", false, "sink writes zeros as fast as possible instead of draining (for -mode thr)")
)

func runSink(addr string) {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		panic(err)
	}
	zeros := make([]byte, 64*1024)
	for {
		c, err := ln.Accept()
		if err != nil {
			continue
		}
		go func(c net.Conn) {
			defer c.Close()
			if *blast {
				for {
					if _, err := c.Write(zeros); err != nil {
						return
					}
				}
			}
			io.Copy(io.Discard, c)
		}(c)
	}
}

// handshake performs one full SOCKS5 negotiation + CONNECT and returns its
// duration. The connection is closed by the deferred Close.
func handshake(tIP net.IP, tPort int) (time.Duration, error) {
	start := time.Now()
	c, err := connect(tIP, tPort)
	if err != nil {
		return 0, err
	}
	c.Close()
	return time.Since(start), nil
}

// connect opens a TCP connection to the proxy, performs the full SOCKS5
// negotiation + CONNECT, and returns the open relay connection (deadline
// cleared) for the caller to use.
func connect(tIP net.IP, tPort int) (net.Conn, error) {
	c, err := net.DialTimeout("tcp", *proxy, 5*time.Second)
	if err != nil {
		return nil, err
	}
	ok := false
	defer func() {
		if !ok {
			c.Close()
		}
	}()
	_ = c.SetDeadline(time.Now().Add(10 * time.Second))

	greet := []byte{0x05, 0x01, 0x00}
	if *user != "" {
		greet = []byte{0x05, 0x01, 0x02}
	}
	if _, err = c.Write(greet); err != nil {
		return nil, err
	}
	sel := make([]byte, 2)
	if _, err = io.ReadFull(c, sel); err != nil {
		return nil, err
	}
	if sel[0] != 0x05 {
		return nil, fmt.Errorf("bad version 0x%02x", sel[0])
	}
	switch sel[1] {
	case 0x00:
	case 0x02:
		req := []byte{0x01, byte(len(*user))}
		req = append(req, *user...)
		req = append(req, byte(len(*pass)))
		req = append(req, *pass...)
		if _, err = c.Write(req); err != nil {
			return nil, err
		}
		ar := make([]byte, 2)
		if _, err = io.ReadFull(c, ar); err != nil {
			return nil, err
		}
		if ar[1] != 0x00 {
			return nil, fmt.Errorf("auth rejected")
		}
	default:
		return nil, fmt.Errorf("no acceptable method 0x%02x", sel[1])
	}

	req := []byte{0x05, 0x01, 0x00, 0x01}
	req = append(req, tIP...)
	req = append(req, byte(tPort>>8), byte(tPort))
	if _, err = c.Write(req); err != nil {
		return nil, err
	}
	rep := make([]byte, 4)
	if _, err = io.ReadFull(c, rep); err != nil {
		return nil, err
	}
	if rep[1] != 0x00 {
		return nil, fmt.Errorf("connect reply 0x%02x", rep[1])
	}
	var skip int
	switch rep[3] {
	case 0x01:
		skip = 4 + 2
	case 0x04:
		skip = 16 + 2
	case 0x03:
		l := make([]byte, 1)
		if _, err = io.ReadFull(c, l); err != nil {
			return nil, err
		}
		skip = int(l[0]) + 2
	}
	if skip > 0 {
		if _, err = io.ReadFull(c, make([]byte, skip)); err != nil {
			return nil, err
		}
	}
	ok = true
	_ = c.SetDeadline(time.Time{})
	return c, nil
}

func main() {
	flag.Parse()
	if *sink != "" {
		runSink(*sink)
		return
	}

	host, portStr, _ := net.SplitHostPort(*target)
	tIP := net.ParseIP(host).To4()
	if tIP == nil {
		fmt.Println("target must be an IPv4 literal host:port")
		return
	}
	var tPort int
	fmt.Sscanf(portStr, "%d", &tPort)

	switch *mode {
	case "cps":
	case "thr":
		runThr(tIP, tPort)
		return
	case "hold":
		runHold(tIP, tPort)
		return
	default:
		fmt.Println("unknown -mode (want cps | thr | hold)")
		return
	}

	var ok, fail int64
	var lats []time.Duration
	var mu sync.Mutex
	deadline := time.Now().Add(*dur)
	var wg sync.WaitGroup
	start := time.Now()
	for i := 0; i < *conc; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			local := make([]time.Duration, 0, 4096)
			for time.Now().Before(deadline) {
				d, err := handshake(tIP, tPort)
				if err != nil {
					atomic.AddInt64(&fail, 1)
					continue
				}
				atomic.AddInt64(&ok, 1)
				local = append(local, d)
			}
			mu.Lock()
			lats = append(lats, local...)
			mu.Unlock()
		}()
	}
	wg.Wait()
	el := time.Since(start).Seconds()
	sort.Slice(lats, func(i, j int) bool { return lats[i] < lats[j] })
	pct := func(p float64) float64 {
		if len(lats) == 0 {
			return 0
		}
		i := int(float64(len(lats)) * p)
		if i >= len(lats) {
			i = len(lats) - 1
		}
		return float64(lats[i].Microseconds()) / 1000.0
	}
	fmt.Printf("%.0f conn/s  (ok=%d fail=%d, conc=%d, %.1fs)\n", float64(ok)/el, ok, fail, *conc, el)
	fmt.Printf("              handshake ms: p50=%.2f p95=%.2f p99=%.2f max=%.2f\n",
		pct(0.50), pct(0.95), pct(0.99), pct(0.999))
}

// runThr: -c long-lived streams each reading bulk data (sink must run with
// -blast) until the deadline; reports aggregate relay throughput.
func runThr(tIP net.IP, tPort int) {
	var total, fails int64
	deadline := time.Now().Add(*dur)
	var wg sync.WaitGroup
	start := time.Now()
	for i := 0; i < *conc; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			c, err := connect(tIP, tPort)
			if err != nil {
				atomic.AddInt64(&fails, 1)
				return
			}
			defer c.Close()
			buf := make([]byte, 256*1024)
			var n int64
			for time.Now().Before(deadline) {
				_ = c.SetReadDeadline(deadline)
				m, err := c.Read(buf)
				n += int64(m)
				if err != nil {
					break
				}
			}
			atomic.AddInt64(&total, n)
		}()
	}
	wg.Wait()
	el := time.Since(start).Seconds()
	mb := float64(total) / 1048576
	streams := *conc - int(fails)
	per := 0.0
	if streams > 0 {
		per = mb / el / float64(streams)
	}
	fmt.Printf("%.1f MB/s aggregate / %d streams (%.1f MB/s each, fail=%d, %.1fs)\n",
		mb/el, streams, per, fails, el)
}

// runHold: ramp -c connections and hold them open and idle until the deadline
// (RFC 9411 7.5 concurrent-capacity style). Watch the proxy's RSS/fd count
// externally while this holds.
func runHold(tIP net.IP, tPort int) {
	var okN, fails int64
	deadline := time.Now().Add(*dur)
	conns := make(chan net.Conn, *conc)
	var wg sync.WaitGroup
	start := time.Now()
	// Ramp with a bounded number of dialers so the proxy sees a steady ramp,
	// not a thundering herd of SYNs.
	dialers := 64
	if dialers > *conc {
		dialers = *conc
	}
	var next int64
	for i := 0; i < dialers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for atomic.AddInt64(&next, 1) <= int64(*conc) {
				c, err := connect(tIP, tPort)
				if err != nil {
					atomic.AddInt64(&fails, 1)
					continue
				}
				atomic.AddInt64(&okN, 1)
				conns <- c
			}
		}()
	}
	wg.Wait()
	ramped := time.Since(start)
	fmt.Printf("holding %d connections (fail=%d, ramp took %.1fs) until deadline...\n",
		okN, fails, ramped.Seconds())
	time.Sleep(time.Until(deadline))
	close(conns)
	for c := range conns {
		c.Close()
	}
	fmt.Printf("held %d concurrent connections for %.1fs\n",
		okN, (*dur - ramped).Seconds())
}
