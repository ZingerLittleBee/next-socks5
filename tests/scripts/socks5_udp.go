// socks5_udp.go -- a tiny, dependency-free (stdlib-only) SOCKS5 UDP ASSOCIATE
// load client for next-socks5 benchmarking. Two modes:
//
//   client (default): for -d duration with -c concurrent workers, each worker
//     opens one TCP control connection to the proxy, performs a full RFC 1928
//     handshake (+ RFC 1929 auth if -user is set) and a UDP ASSOCIATE, then
//     blasts -size byte datagrams (SOCKS5-encapsulated, RFC 1928 section 7)
//     at -target via the advertised relay and counts the echoes coming back.
//     Reports pps each way, echoed goodput, end-to-end drop rate, and RTT
//     percentiles (each payload carries its send timestamp). The echo path
//     exercises BOTH relay directions: client->target (decap, resolve, egress
//     check, send) and target->client (source filter, encap, send).
//
//   sink (-sink ADDR): a UDP echo server used as the relay target, so the
//     upstream never bottlenecks.
package main

import (
	"encoding/binary"
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
	target = flag.String("target", "127.0.0.1:19091", "UDP echo target (IPv4 literal host:port)")
	conc   = flag.Int("c", 32, "concurrent workers (one association each)")
	dur    = flag.Duration("d", 10*time.Second, "test duration")
	size   = flag.Int("size", 64, "datagram payload bytes (min 16; try 64 and 1400)")
	rate   = flag.Int("rate", 0, "per-worker send rate cap in pps (0 = unpaced)")
	sink   = flag.String("sink", "", "run as a UDP echo sink on this addr instead of a client")
)

func runSink(addr string) {
	pc, err := net.ListenPacket("udp", addr)
	if err != nil {
		panic(err)
	}
	buf := make([]byte, 65536)
	for {
		n, src, err := pc.ReadFrom(buf)
		if err != nil {
			continue
		}
		_, _ = pc.WriteTo(buf[:n], src)
	}
}

// associate performs the SOCKS5 negotiation + UDP ASSOCIATE on a fresh TCP
// control connection and returns it (kept open: the association lives only as
// long as the control connection) plus the advertised relay address.
func associate() (net.Conn, *net.UDPAddr, error) {
	c, err := net.DialTimeout("tcp", *proxy, 5*time.Second)
	if err != nil {
		return nil, nil, err
	}
	_ = c.SetDeadline(time.Now().Add(10 * time.Second))

	greet := []byte{0x05, 0x01, 0x00}
	if *user != "" {
		greet = []byte{0x05, 0x01, 0x02}
	}
	if _, err = c.Write(greet); err != nil {
		c.Close()
		return nil, nil, err
	}
	sel := make([]byte, 2)
	if _, err = io.ReadFull(c, sel); err != nil {
		c.Close()
		return nil, nil, err
	}
	switch sel[1] {
	case 0x00:
	case 0x02:
		req := []byte{0x01, byte(len(*user))}
		req = append(req, *user...)
		req = append(req, byte(len(*pass)))
		req = append(req, *pass...)
		if _, err = c.Write(req); err != nil {
			c.Close()
			return nil, nil, err
		}
		ar := make([]byte, 2)
		if _, err = io.ReadFull(c, ar); err != nil {
			c.Close()
			return nil, nil, err
		}
		if ar[1] != 0x00 {
			c.Close()
			return nil, nil, fmt.Errorf("auth rejected")
		}
	default:
		c.Close()
		return nil, nil, fmt.Errorf("no acceptable method 0x%02x", sel[1])
	}

	// UDP ASSOCIATE with DST = 0.0.0.0:0 (client address not known yet).
	req := []byte{0x05, 0x03, 0x00, 0x01, 0, 0, 0, 0, 0, 0}
	if _, err = c.Write(req); err != nil {
		c.Close()
		return nil, nil, err
	}
	rep := make([]byte, 4)
	if _, err = io.ReadFull(c, rep); err != nil {
		c.Close()
		return nil, nil, err
	}
	if rep[1] != 0x00 {
		c.Close()
		return nil, nil, fmt.Errorf("associate reply 0x%02x", rep[1])
	}
	var bndIP net.IP
	switch rep[3] {
	case 0x01:
		b := make([]byte, 4+2)
		if _, err = io.ReadFull(c, b); err != nil {
			c.Close()
			return nil, nil, err
		}
		bndIP = net.IP(b[:4])
		port := int(b[4])<<8 | int(b[5])
		if bndIP.IsUnspecified() {
			host, _, _ := net.SplitHostPort(*proxy)
			bndIP = net.ParseIP(host)
		}
		return c, &net.UDPAddr{IP: bndIP, Port: port}, nil
	case 0x04:
		b := make([]byte, 16+2)
		if _, err = io.ReadFull(c, b); err != nil {
			c.Close()
			return nil, nil, err
		}
		return c, &net.UDPAddr{IP: net.IP(b[:16]), Port: int(b[16])<<8 | int(b[17])}, nil
	default:
		c.Close()
		return nil, nil, fmt.Errorf("unexpected BND ATYP 0x%02x", rep[3])
	}
}

// encap builds RSV(0x0000) FRAG(0x00) ATYP(0x01) DST.ADDR DST.PORT + payload.
func encap(tIP net.IP, tPort int, payload []byte) []byte {
	out := make([]byte, 0, 10+len(payload))
	out = append(out, 0x00, 0x00, 0x00, 0x01)
	out = append(out, tIP...)
	out = append(out, byte(tPort>>8), byte(tPort))
	return append(out, payload...)
}

// decapPayload returns the payload of a SOCKS5 UDP datagram, or nil if the
// header is malformed / fragmented.
func decapPayload(b []byte) []byte {
	if len(b) < 4 || b[2] != 0x00 {
		return nil
	}
	var hdr int
	switch b[3] {
	case 0x01:
		hdr = 4 + 4 + 2
	case 0x04:
		hdr = 4 + 16 + 2
	case 0x03:
		if len(b) < 5 {
			return nil
		}
		hdr = 5 + int(b[4]) + 2
	default:
		return nil
	}
	if len(b) < hdr {
		return nil
	}
	return b[hdr:]
}

func main() {
	flag.Parse()
	if *sink != "" {
		runSink(*sink)
		return
	}
	if *size < 16 {
		fmt.Println("-size must be >= 16 (payload carries a seq + timestamp)")
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

	var sent, recv, assocFail int64
	var lats []time.Duration
	var mu sync.Mutex
	deadline := time.Now().Add(*dur)
	var wg sync.WaitGroup
	start := time.Now()

	for i := 0; i < *conc; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			ctrl, bnd, err := associate()
			if err != nil {
				atomic.AddInt64(&assocFail, 1)
				return
			}
			defer ctrl.Close()
			us, err := net.DialUDP("udp", nil, bnd)
			if err != nil {
				atomic.AddInt64(&assocFail, 1)
				return
			}
			defer us.Close()

			// Receiver: count echoes and sample RTT from the embedded
			// send timestamp; a short grace period after the send
			// deadline catches in-flight datagrams.
			local := make([]time.Duration, 0, 65536)
			var rwg sync.WaitGroup
			rwg.Add(1)
			go func() {
				defer rwg.Done()
				buf := make([]byte, 65536)
				for {
					_ = us.SetReadDeadline(deadline.Add(500 * time.Millisecond))
					n, err := us.Read(buf)
					if err != nil {
						return
					}
					p := decapPayload(buf[:n])
					if len(p) < 16 {
						continue
					}
					atomic.AddInt64(&recv, 1)
					sentNs := int64(binary.BigEndian.Uint64(p[8:16]))
					local = append(local, time.Duration(time.Now().UnixNano()-sentNs))
				}
			}()

			payload := make([]byte, *size)
			dgram := encap(tIP, tPort, payload)
			body := dgram[10:] // payload region inside the frame
			var seq uint64
			var tick *time.Ticker
			if *rate > 0 {
				tick = time.NewTicker(time.Second / time.Duration(*rate))
				defer tick.Stop()
			}
			for time.Now().Before(deadline) {
				seq++
				binary.BigEndian.PutUint64(body[0:8], seq)
				binary.BigEndian.PutUint64(body[8:16], uint64(time.Now().UnixNano()))
				if _, err := us.Write(dgram); err == nil {
					atomic.AddInt64(&sent, 1)
				}
				if tick != nil {
					<-tick.C
				}
			}
			rwg.Wait()
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
	drop := 0.0
	if sent > 0 {
		drop = 100.0 * float64(sent-recv) / float64(sent)
	}
	fmt.Printf("sent %.0f pps, echoed %.0f pps  (sent=%d recv=%d drop=%.1f%%, conc=%d, assoc_fail=%d, %.1fs)\n",
		float64(sent)/el, float64(recv)/el, sent, recv, drop, *conc, assocFail, el)
	fmt.Printf("              echoed goodput: %.1f MB/s (%d B payload)\n",
		float64(recv)*float64(*size)/el/1048576, *size)
	fmt.Printf("              rtt ms: p50=%.2f p95=%.2f p99=%.2f max=%.2f\n",
		pct(0.50), pct(0.95), pct(0.99), pct(0.999))
}
