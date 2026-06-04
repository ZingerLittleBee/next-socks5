// socks5_cps.go -- a tiny, dependency-free (stdlib-only) SOCKS5 load client for
// next-socks5 benchmarking. Two modes:
//
//   client (default): for -d duration with -c concurrent workers, each worker
//     repeatedly opens a TCP connection to the proxy, performs a full RFC 1928
//     handshake (+ RFC 1929 auth if -user is set) and a CONNECT to -target, then
//     closes immediately. Reports connection-establishment rate (CPS, RFC 3511
//     5.3) and handshake-latency percentiles. This is the accurate CPS tool that
//     curl/proxychains cannot be (no per-request process spawn).
//
//   sink (-sink ADDR): a lightweight TCP accept-and-drain server used as the
//     CONNECT target for the CPS test, so the upstream never bottlenecks.
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
	sink   = flag.String("sink", "", "run as a TCP sink on this addr instead of a client")
)

func runSink(addr string) {
	ln, err := net.Listen("tcp", addr)
	if err != nil {
		panic(err)
	}
	for {
		c, err := ln.Accept()
		if err != nil {
			continue
		}
		go func(c net.Conn) { io.Copy(io.Discard, c); c.Close() }(c)
	}
}

// handshake performs one full SOCKS5 negotiation + CONNECT and returns its
// duration. The connection is closed by the deferred Close.
func handshake(tIP net.IP, tPort int) (time.Duration, error) {
	start := time.Now()
	c, err := net.DialTimeout("tcp", *proxy, 5*time.Second)
	if err != nil {
		return 0, err
	}
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(10 * time.Second))

	greet := []byte{0x05, 0x01, 0x00}
	if *user != "" {
		greet = []byte{0x05, 0x01, 0x02}
	}
	if _, err = c.Write(greet); err != nil {
		return 0, err
	}
	sel := make([]byte, 2)
	if _, err = io.ReadFull(c, sel); err != nil {
		return 0, err
	}
	if sel[0] != 0x05 {
		return 0, fmt.Errorf("bad version 0x%02x", sel[0])
	}
	switch sel[1] {
	case 0x00:
	case 0x02:
		req := []byte{0x01, byte(len(*user))}
		req = append(req, *user...)
		req = append(req, byte(len(*pass)))
		req = append(req, *pass...)
		if _, err = c.Write(req); err != nil {
			return 0, err
		}
		ar := make([]byte, 2)
		if _, err = io.ReadFull(c, ar); err != nil {
			return 0, err
		}
		if ar[1] != 0x00 {
			return 0, fmt.Errorf("auth rejected")
		}
	default:
		return 0, fmt.Errorf("no acceptable method 0x%02x", sel[1])
	}

	req := []byte{0x05, 0x01, 0x00, 0x01}
	req = append(req, tIP...)
	req = append(req, byte(tPort>>8), byte(tPort))
	if _, err = c.Write(req); err != nil {
		return 0, err
	}
	rep := make([]byte, 4)
	if _, err = io.ReadFull(c, rep); err != nil {
		return 0, err
	}
	if rep[1] != 0x00 {
		return 0, fmt.Errorf("connect reply 0x%02x", rep[1])
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
			return 0, err
		}
		skip = int(l[0]) + 2
	}
	if skip > 0 {
		if _, err = io.ReadFull(c, make([]byte, skip)); err != nil {
			return 0, err
		}
	}
	return time.Since(start), nil
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
