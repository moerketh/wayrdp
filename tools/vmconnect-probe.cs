using System;
using System.Collections.Generic;
using System.Linq;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Threading;

// vmconnect-probe - faithful vmconnect (Enhanced Session) wire emulator for
// the wayrdp RDP server, over AF_HYPERV (Hyper-V socket).
//
// Why this exists: the server puts vsock peers into VmConnectMode
// (RdpSecurity=1, NlaSecurity=0, VmConnectMode=1, EncryptionMethods=NONE).
// FreeRDP's vsock transport then SHORT-CIRCUITS its TLS/nego accept and
// expects the client to do exactly what real vmconnect does:
//   1. TPKT + X.224 Connection Request with NO RDP_NEG_REQ (cookie: none).
//   2. On X.224 CC, send a raw MCS Connect-Initial (BER, GCC CCrq).
// Generic clients (xfreerdp3) instead send an X.224 RDP_NEG_REQ and then
// hang forever waiting for a nego response the server never sends. This is
// why the xfreerdp-based harness stalls pre-activation and cannot PASS.
// This prober speaks vmconnect's handshake natively and ASSERTS each stage.
//
// Assertion contract (mirrors the handshake; each stage FAILs loudly):
//   S1 connect   — AF_HYPERV WSAConnect to the VM's RDP service GUID.
//   S2 x224-cc   — server replies to our CR with TPKT+X.224 CC.
//   S3 mcs-resp  — server replies to MCS Connect-Initial with a TPKT bearing
//                  an MCS Connect-Response (BER app tag 0xa1/0xa2 family).
//
// Usage: vmconnect-probe.exe <vmGuid> <serviceGuid> [--seconds N]
// Exit: 0 = all asserted stages passed; 2 = a stage failed (name printed).
//
// Wire bytes are hand-rolled (no RDP stack) so the prober is independent of
// FreeRDP and can catch FreeRDP-side regressions on the server.

public class VmConnectProbe {
    [DllImport("ws2_32.dll", SetLastError=true)]
    static extern IntPtr WSASocketW(int af, int type, int protocol, IntPtr lpProtocolInfo, uint g, uint dwFlags);
    [DllImport("ws2_32.dll", SetLastError=true)]
    static extern int WSAConnect(IntPtr s, byte[] name, int namelen, IntPtr a, IntPtr b, IntPtr c, IntPtr d);
    [DllImport("ws2_32.dll")] static extern int closesocket(IntPtr s);
    [DllImport("ws2_32.dll")] static extern int send(IntPtr s, byte[] buf, int len, int flags);
    [DllImport("ws2_32.dll")] static extern int recv(IntPtr s, byte[] buf, int len, int flags);
    [DllImport("ws2_32.dll")] static extern int shutdown(IntPtr s, int how);
    [DllImport("ws2_32.dll", SetLastError=true)] static extern int WSAGetLastError();
    [DllImport("ws2_32.dll", SetLastError=true)] static extern int WSAStartup(ushort wVersionRequested, IntPtr lpWSAData);

    const int AF_HYPERV = 34, SOCK_STREAM = 1, HV_PROTOCOL_RAW = 1;
    const int SD_BOTH = 2;
    static int stage = 0;

    static IntPtr ConnectHv(Guid vmId, Guid svc) {
        IntPtr raw = WSASocketW(AF_HYPERV, SOCK_STREAM, HV_PROTOCOL_RAW, IntPtr.Zero, 0, 0);
        if (raw.ToInt64() < 0) throw new Exception("WSASocketW failed " + WSAGetLastError());
        byte[] buf = new byte[36];
        BitConverter.GetBytes((ushort)AF_HYPERV).CopyTo(buf, 0);
        vmId.ToByteArray().CopyTo(buf, 4);
        svc.ToByteArray().CopyTo(buf, 20);
        if (WSAConnect(raw, buf, buf.Length, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero) != 0) {
            int e = WSAGetLastError(); closesocket(raw);
            throw new Exception("WSAConnect failed " + e);
        }
        return raw;
    }

    static void Fail(string name, string detail) {
        Console.WriteLine("[probe] FAIL stage=" + name + " :: " + detail);
        Environment.Exit(2);
    }

    // A8d mid-activation kill hook: when --die-at-stage N is given, the prober
    // TerminateProcess()es itself (no finally, no socket finalizer — the same
    // death as Stop-Process -Force / SIGKILL) right after Pass() marks stage
    // N. A timed kill from PowerShell lands wherever the hold loop happens to
    // be (post-activation); this dies INSIDE the handshake so wayrdp sees the
    // vsock vanish mid-activation — the wedge the peer-teardown fix targets.
    [System.Runtime.InteropServices.DllImport("kernel32.dll")]
    static extern bool TerminateProcess(IntPtr hProcess, uint uExitCode);
    static int dieAtStage = 0;

    static void Pass(string name, string detail) {
        stage++;
        Console.WriteLine("[probe] PASS stage=" + stage + " " + name + " :: " + detail);
        if (dieAtStage > 0 && stage == dieAtStage) {
            Console.WriteLine("[probe] DIE-AT-STAGE " + dieAtStage + " — TerminateProcess, socket left open");
            Console.Out.Flush();
            TerminateProcess(new IntPtr(-1), 7);
        }
    }

    // Send all bytes or die.
    static void SendAll(IntPtr s, byte[] buf) {
        int off = 0;
        while (off < buf.Length) {
            int w = send(s, buf, buf.Length - off, 0);
            if (w <= 0) { int e = WSAGetLastError(); Fail("send", "WSA error " + e + " after " + off + "/" + buf.Length); }
            // send() on a non-blocking-ish raw socket may short-write; we keep
            // the remainder. (Raw HV socket here is blocking by default.)
            buf = buf.Skip(w).ToArray();
        }
    }

    // Receive up to buf.Length bytes with a wall-clock timeout; returns count.
    static int RecvTimeout(IntPtr s, byte[] buf, int timeoutMs) {
        var deadline = Environment.TickCount + timeoutMs;
        while (Environment.TickCount < deadline) {
            int n = recv(s, buf, buf.Length, 0);
            if (n > 0) return n;
            if (n == 0) return 0; // peer closed
            int e = WSAGetLastError();
            if (e != 10035 && e != 10060) Fail("recv", "WSA error " + e);
            Thread.Sleep(10);
        }
        return -1; // timeout
    }

    // --- X.224 / TPKT helpers -------------------------------------------
    // TPKT: 03 00 <len-hi> <len-lo> | X.224: <li> E0(CR)/D0(CC) 00 00 00 00 00
    static byte[] BuildX224Cr() {
        // X.224 CR, fixed 6-byte header, dst-ref=0 src-ref=0 class=0, NO
        // RDP_NEG_REQ (that's the whole point — vmconnect does not negotiate).
        // TPKT length = 4 (tpkt) + 7 (x224: li + 6 bytes) = 11.
        return new byte[] {
            0x03, 0x00, 0x00, 0x0b,             // TPKT v3, len=11
            0x06, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00  // LI=6, CR, refs=0, class=0
        };
    }

    // --- FreeRDP-equivalent PER writers (byte-stream ops, not bit-packed) --
    // Mirrors libfreerdp/crypto/per.c. FreeRDP's PER is byte-length based,
    // so these are simple emitters, validated against per.c reads.
    static List<byte> PerLen(int len) {
        var o = new List<byte>();
        if (len > 0x7F) { o.Add((byte)((len >> 8) | 0x80)); o.Add((byte)(len & 0xff)); }
        else o.Add((byte)len);
        return o;
    }
    static void PerOctetString(List<byte> s, byte[] v, int min) {
        int mlen = (v.Length >= min) ? v.Length - min : min;
        s.AddRange(PerLen(mlen)); s.AddRange(v);
    }
    // T.124 OID: 0,0,20,124,0,1 -> tuples packed: 0*40+0=0, 20,124,0,1
    static readonly byte[] T124_OID = new byte[]{ 0, 0, 20, 124, 0, 1 };
    static void PerOid(List<byte> s, byte[] oid) {
        s.Add(5); s.Add((byte)(oid[0]*40 + oid[1])); s.Add(oid[2]); s.Add(oid[3]); s.Add(oid[4]); s.Add(oid[5]);
    }
    static void PerNumericString(List<byte> s, byte[] str, int min) {
        int mlen = (str.Length >= min) ? str.Length - min : min;
        s.AddRange(PerLen(mlen));
        for (int i = 0; i < str.Length; i += 2) {
            int c1 = str[i] - 0x30; int c2 = (i + 1 < str.Length) ? str[i+1] - 0x30 : 0;
            s.Add((byte)(((c1 % 10) << 4) | (c2 % 10)));
        }
    }

    // MCS Connect-Initial. userData = PER ConferenceCreateRequest wrapping
    // GCC client data blocks (CS_CORE + CS_SECURITY), matching what
    // FreeRDP's gcc_write_conference_create_request emits byte-for-byte so
    // rdp_server_accept_mcs_connect_initial -> gcc_read_conference_create_request
    // accepts it. VmConnectMode means no encryption, so securityMethods=0.
    static byte[] BuildMcsConnectInitial() {
        // --- GCC client data blocks (the PER userData value) -------------
        // clientCoreData (216 bytes wired; FreeRDP pads clientCoreData to the
        // RDP 4.0 baseline). Only fields the server actually reads matter;
        // version=RDP 5 (0x0A) keeps EGFX-capable paths available.
        var core = new byte[216];
        core[0] = 0x0a; core[1] = 0x00; core[2] = 0x08; core[3] = 0x00; // version RDP5
        core[4] = 0x20; core[5] = 0x03;              // desktopWidth 800
        core[6] = 0x58; core[7] = 0x02;              // desktopHeight 600
        core[8] = 0x01; core[9] = 0xca;              // colorDepth 0xca01
        core[10] = 0xaa; core[11] = 0x03;            // sasSequence
        core[12] = 0x09; core[13] = 0x04;            // keyboardLayout en-US
        core[16] = 0x00; core[17] = 0x04;            // clientBuild 1024
        // clientName "WAYRDPPROBE" utf16-le at [32..] (left as zero ok)
        core[208] = 0x18; core[209] = 0x00;          // highColorDepth 24
        core[210] = 0x07; core[211] = 0x00;          // supportedColorDepths
        core[212] = 0x01; core[213] = 0x00;          // earlyCapabilityFlags

        var blocks = new List<byte>();
        // CS_CORE: 0xC001, length = 4 (hdr) + payload
        blocks.Add(0x01); blocks.Add(0xc0);
        blocks.Add((byte)((4 + core.Length) & 0xff)); blocks.Add((byte)((4 + core.Length) >> 8));
        blocks.AddRange(core);
        // CS_SECURITY: 0xC002, clientSecurityData = encryptionMethods(4 LE)
        // + extEncryptionMethods(4 LE) = 8 payload bytes; block length 4+8=12.
        blocks.Add(0x02); blocks.Add(0xc0);
        blocks.Add(0x0c); blocks.Add(0x00);            // length = 12 (LE)
        blocks.AddRange(new byte[8]);                  // both methods = 0 (none)
        // CS_NET: declare "drdynvc" so the server registers the DVC manager
        // and assigns it a static channel id (returned in the Connect-Response
        // SC_NET block; we parse it from the hex below).
        var drdynvc = new byte[12];                    // channelDef: name[8] + options(4)
        Array.Copy(System.Text.Encoding.ASCII.GetBytes("drdynvc\0"), drdynvc, 8);
        drdynvc[8] = 0x00; drdynvc[9] = 0x00; drdynvc[10] = 0x80; drdynvc[11] = 0x00; // CHANNEL_OPTION_INITIALIZED
        int csnetLen = 4 + 4 + drdynvc.Length;         // hdr(4) + channelCount(4) + defs
        blocks.Add(0x03); blocks.Add(0xc0);            // CS_NET tag (LE)
        blocks.Add((byte)(csnetLen & 0xff)); blocks.Add((byte)(csnetLen >> 8));
        AddU32(blocks, 1);                             // channelCount (UINT32 per gcc.c)
        blocks.AddRange(drdynvc);

        // --- PER ConferenceCreateRequest (per.c semantics) ----------------
        var gcc = new List<byte>();
        gcc.Add(0x00);                                 // ConnectData::choice object(0)
        PerOid(gcc, T124_OID);                         // T.124 OID
        gcc.AddRange(PerLen(blocks.Count + 14));       // connectPDU length
        gcc.Add(0x00);                                 // ConnectGCCPDU::choice conferenceCreateRequest(0)
        gcc.Add(0x08);                                 // selection: userData present
        PerNumericString(gcc, new byte[]{0x31}, 1);    // conferenceName "1"
        gcc.Add(0x00);                                 // padding(1)
        gcc.Add(0x01);                                 // numberOfSets = 1
        gcc.Add(0xc0);                                 // UserData::choice h221NonStandard(1)
        var h221 = new byte[]{ 0x44, 0x75, 0x63, 0x61 }; // "Duca" client->server
        PerOctetString(gcc, h221, 4);                  // h221NonStandard
        PerOctetString(gcc, blocks.ToArray(), 0);      // userData::value = client blocks

        // --- MCS Connect-Initial (BER APPLICATION 0 / 0xa0) ---------------
        var body = new List<byte>();
        body.AddRange(BerOctetString(new byte[]{0x01}));        // callingDomainSelector
        body.AddRange(BerOctetString(new byte[]{0x01}));        // calledDomainSelector
        body.AddRange(BerBoolean(true));                        // upwardFlag = TRUE
        // targetParameters
        var target = new List<byte>();
        target.AddRange(BerInteger(new byte[]{0x22}));          // maxChannelIds = 34
        target.AddRange(BerInteger(new byte[]{2}));             // maxUserIds = 2
        target.AddRange(BerInteger(new byte[]{0}));             // maxTokenIds = 0
        target.AddRange(BerInteger(new byte[]{1}));             // numPriorities = 1
        target.AddRange(BerInteger(new byte[]{0}));             // minThroughput = 0
        target.AddRange(BerInteger(new byte[]{1}));             // maxHeight = 1
        target.AddRange(BerInteger(new byte[]{0xff,0xff}));     // maxMCSPDUsize = 65535
        target.AddRange(BerInteger(new byte[]{2}));             // protocolVersion = 2
        body.AddRange(BerSequence(target));
        // minimumParameters
        var minimum = new List<byte>();
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{1}));
        minimum.AddRange(BerInteger(new byte[]{0x42,0x00}));
        minimum.AddRange(BerInteger(new byte[]{2}));
        body.AddRange(BerSequence(minimum));
        // maximumParameters
        var maximum = new List<byte>();
        maximum.AddRange(BerInteger(new byte[]{0xff,0xff}));
        maximum.AddRange(BerInteger(new byte[]{0xfc,0x17}));
        maximum.AddRange(BerInteger(new byte[]{0xff,0xff}));
        maximum.AddRange(BerInteger(new byte[]{1}));
        maximum.AddRange(BerInteger(new byte[]{0}));
        maximum.AddRange(BerInteger(new byte[]{1}));
        maximum.AddRange(BerInteger(new byte[]{0xff,0xff}));
        maximum.AddRange(BerInteger(new byte[]{2}));
        body.AddRange(BerSequence(maximum));
        body.AddRange(BerOctetString(gcc.ToArray()));           // userData
        var mcs = McsConnectInitial(body);                      // 7f 65 = MCS Connect-Initial (App 101)

        // After X.224 CC, TPKT data PDUs carry an X.224 Data header:
        //   LI=0x02, code = 0xF0 (X224_TPDU_DATA) | EOT = 0x80, TPDU-NR=0x00.
        // FreeRDP's tpdu_read_data requires code 0xF0; omitting this header
        // makes it read our BER length byte (0x82) as the code and fail.
        var dataHeader = new byte[]{ 0x02, 0xf0, 0x80 };

        // TPKT wrap: 4 (tpkt) + 3 (x224 data header) + mcs
        var tpkt = new List<byte>();
        tpkt.Add(0x03); tpkt.Add(0x00);
        int total = 4 + dataHeader.Length + mcs.Count;
        tpkt.Add((byte)((total >> 8) & 0xff)); tpkt.Add((byte)(total & 0xff));
        tpkt.AddRange(dataHeader);
        tpkt.AddRange(mcs);
        return tpkt.ToArray();
    }

    // --- tiny BER encoders ----------------------------------------------
    static byte[] BerLen(int len) {
        if (len < 0x80) return new byte[]{ (byte)len };
        if (len < 0x100) return new byte[]{ 0x81, (byte)len };
        return new byte[]{ 0x82, (byte)((len>>8)&0xff), (byte)(len&0xff) };
    }
    static List<byte> Wrap(byte tag, IEnumerable<byte> payload) {
        var p = payload.ToList();
        var o = new List<byte>(); o.Add(tag); o.AddRange(BerLen(p.Count)); o.AddRange(p); return o;
    }
    static List<byte> BerOctetString(byte[] v) { return Wrap(0x04, v); }
    static List<byte> BerInteger(byte[] v) { return Wrap(0x02, v); }
    static List<byte> BerBoolean(bool v) { return Wrap(0x01, new byte[]{ v ? (byte)0xff : (byte)0x00 }); }
    static List<byte> BerSequence(IEnumerable<byte> v) { return Wrap(0x30, v); }
    // MCS domainMCSPDU tags are NOT plain BER [APPLICATION n]: FreeRDP's
    // ber_read_application_tag expects the T.125 selector byte 0x7f followed
    // by the tag number. Connect-Initial is Application 101 (0x65).
    static List<byte> McsConnectInitial(IEnumerable<byte> v) {
        var p = v.ToList();
        var o = new List<byte>(); o.Add(0x7f); o.Add(0x65); o.AddRange(BerLen(p.Count)); o.AddRange(p);
        return o;
    }

    // --- MCS ladder PDU builders -------------------------------------------
    // All wrapped as TPKT + X.224-Data(02 f0 80) + MCS PDU.
    static byte[] Tpkt(params byte[][] parts) {
        var payload = new List<byte>(); foreach (var p in parts) payload.AddRange(p);
        payload.InsertRange(0, new byte[]{ 0x02, 0xf0, 0x80 });   // X.224 Data header
        var o = new List<byte>(){ 0x03, 0x00, (byte)((4+payload.Count)>>8), (byte)((4+payload.Count)&0xff) };
        o.AddRange(payload); return o.ToArray();
    }
    // ErectDomainRequest ::= [APPLICATION 1] { subHeight(0), subInterval(0) }
    // MCS ladder PDUs (ErectDomain on) use a single PER choice byte = tag<<2|opts,
    // then PER integers (length-prefix + big-endian payload), NOT BER.
    // per_write_integer(0) -> per_write_length(1)=0x01 then 0x00  =>  { 0x01, 0x00 }
    static byte[] ErectDomainRequest() {
        return Tpkt(new byte[]{ 0x04, 0x01, 0x00, 0x01, 0x00 });
    }
    // PER write for channel-join / send-data integers is already big-endian, no
    // length prefix when written via per_write_integer16 (plain 2-byte BE).
    static byte[] AttachUserRequest() { return Tpkt(new byte[]{ 0x28 }); }            // App10 = (10<<2) = 0x28
    static byte[] ChannelJoinRequest(int initiator, int channelId) {
        return Tpkt(new byte[]{ 0x38, (byte)(initiator>>8), (byte)initiator, (byte)(channelId>>8), (byte)channelId });
    }

    // Buffered stream reader: pull TPKTs one at a time (a recv may carry a
    // partial TPKT or several). Returns the MCS tag byte (0x7f 0x66 style for
    // connect-response, or the top-byte for ladder PDUs) + full TPKT payload.
    class HvStream {
        IntPtr s; List<byte> backlog = new List<byte>();
        public HvStream(IntPtr sock) { s = sock; }
        byte[] Fill(int need, int timeoutMs) {
            int deadline = Environment.TickCount + timeoutMs; var tmp = new byte[8192];
            while (backlog.Count < need && Environment.TickCount < deadline) {
                int n = recv(s, tmp, tmp.Length, 0);
                if (n > 0) { for (int i = 0; i < n; i++) backlog.Add(tmp[i]); }
                else if (n == 0) return null;
                else { int e = WSAGetLastError(); if (e != 10035 && e != 10060) return null; Thread.Sleep(5); }
            }
            if (backlog.Count < need) return null;
            var o = backlog.GetRange(0, need); backlog.RemoveRange(0, need); return o.ToArray();
        }
        public byte[] ReadTpkt(int timeoutMs) {
            var hdr = Fill(4, timeoutMs); if (hdr == null || hdr[0] != 0x03) return null;
            int len = (hdr[2] << 8) | hdr[3];
            // Guard against a malformed short TPKT length: len < 4 would
            // drive Fill negative and throw. Treat it as an empty-body TPKT
            // so the caller sees the header and moves on rather than dying.
            if (len < 4) return (byte[])hdr.Clone();
            var rest = Fill(len - 4, timeoutMs);
            if (rest == null) return null;
            var full = new List<byte>(hdr); full.AddRange(rest); return full.ToArray();
        }
    }

    // Return the offset of the Share Control Header inside an MCS
    // SendDataIndication TPKT (per rdp_read_header in FreeRDP rdp.c):
    // TPKT(4)+X.224 hdr(3)+PER choice(1)+initiator(2)+channelId(2)
    // +dataPriority/segmentation(1)+PER userData length(1 or 2, long form
    // when the length byte has the 0x80 bit set).
    static int SdiUserDataOffset(byte[] p) {
        int i = 13;
        if (p.Length <= i) return -1;
        if ((p[i] & 0x80) != 0) i += 2; else i += 1;
        return (p.Length > i + 5) ? i : -1;
    }

    public static int Main(string[] args) {
        if (args.Length < 2) { Console.WriteLine("usage: vmconnect-probe <vmGuid> <serviceGuid> [--seconds N] [--send-input] [--die-at-stage N]"); return 2; }
        int settleSec = 2;
        for (int i = 2; i < args.Length; i++) {
            if (args[i] == "--seconds" && i + 1 < args.Length) settleSec = int.Parse(args[++i]);
            else if (args[i] == "--send-input") sendInput = true;
            else if (args[i] == "--die-at-stage" && i + 1 < args.Length) dieAtStage = int.Parse(args[++i]);
        }
        Guid vmId = new Guid(args[0]);
        Guid svc = new Guid(args[1]);
        WSAStartup(0x0202, Marshal.AllocHGlobal(512));
        // Hard watchdog: a blocking recv under a continuous server flood can
        // outlast the per-read deadlines (Fill returns data each call, so the
        // hold loop's deadline is the only bound, and a mid-gate read has
        // none). If the whole run exceeds settleSec + 60s we are wedged; force
        // exit with a distinctive marker so the harness reads it as a hang,
        // not a pass.
        var wd = new System.Threading.Timer(_ => {
            Console.WriteLine("[probe] WATCHDOG-EXCEEDED — forcing exit (was blocked in a read)");
            Environment.Exit(3);
        }, null, (settleSec + 60) * 1000, System.Threading.Timeout.Infinite);
        IntPtr s = IntPtr.Zero;
        try {
            // S1 connect
            s = ConnectHv(vmId, svc);
            Pass("connect", "AF_HYPERV connected to " + vmId + " svc " + svc);

            // S2 X.224 CC
            SendAll(s, BuildX224Cr());
            var buf = new byte[4096];
            int n = RecvTimeout(s, buf, 8000);
            if (n <= 0) Fail("x224-cc", "no CC within 8s (recv=" + n + ")");
            // TPKT(03 00 00 len) then X.224 CC: LI is buf[4] (0x06 minimal,
            // often 0x0E when the server appends routing/response tokens),
            // and 0xD0 = CC. Don't pin LI; accept the D0-type PDU.
            if (n >= 6 && buf[0] == 0x03 && buf[5] == 0xd0) {
                Pass("x224-cc", "server sent Connection Confirm (len=" + n + ")");
            } else {
                Fail("x224-cc", "unexpected bytes: " + BitConverter.ToString(buf, 0, Math.Min(n, 16)));
            }

            // S3 MCS Connect-Response
            SendAll(s, BuildMcsConnectInitial());
            var stream = new HvStream(s);
            var resp = stream.ReadTpkt(10000);
            if (resp == null) Fail("mcs-resp", "no Connect-Response within 10s");
            // TPKT: 03 00 len len | X224 data hdr 02 f0 80 | MCS 7f 66 ...
            int mcsOff = 7; // 4 (tpkt) + 3 (x224 data hdr)
            if (!(resp.Length >= mcsOff + 2 && resp[mcsOff] == 0x7f && resp[mcsOff+1] == 0x66))
                Fail("mcs-resp", "unexpected MCS tag: " + BitConverter.ToString(resp, 0, Math.Min(resp.Length, 12)));
            Pass("mcs-resp", "server sent MCS Connect-Response");
            Console.WriteLine("[probe][dbg] CCrsp hex (" + resp.Length + "B): " + BitConverter.ToString(resp, 7, Math.Min(resp.Length - 7, 400)));
            // Fish the SC_NET block (type 0x0C03 LE) out of the GCC user data
            // to learn the assigned channel ids. Layout: IOMCSChannelId(2 LE)
            // channelCount(2 LE) channelId[channelCount](2 LE each).
            int drdynvcChan = -1;
            for (int i = 0; i + 7 < resp.Length; i++) {
                if (resp[i] == 0x03 && resp[i+1] == 0x0c) {
                    int iomcs = resp[i+4] | (resp[i+5] << 8);
                    int cnt = resp[i+6] | (resp[i+7] << 8);
                    Console.WriteLine("[probe][dbg] SC_NET io=" + iomcs + " channels=" + cnt);
                    if (cnt >= 1) drdynvcChan = resp[i+8] | (resp[i+9] << 8);
                    break;
                }
            }
            if (drdynvcChan < 0) Fail("mcs-resp", "server assigned no drdynvc channel in SC_NET");

            // --- S4: MCS ladder to activation ------------------------------
            // --- S4: MCS ladder to activation ------------------------------
            SendAll(s, ErectDomainRequest());
            SendAll(s, AttachUserRequest());
            var confirm = stream.ReadTpkt(8000);
            if (confirm == null) Fail("attach-user", "no AttachUserConfirm");
            int userId = -1;
            if (confirm.Length > 7 && confirm[7] == 0x2e) {
                userId = (confirm[confirm.Length-2] << 8) | confirm[confirm.Length-1];
                Pass("attach-user", "userId=" + userId);
            } else {
                Fail("attach-user", "expected AttachUserConfirm(0x2e), got 0x" + confirm[7].ToString("x2"));
            }
            // ChannelJoin: the initiator field is a UserId written PER-
            // integer16 with base MCS_BASE_CHANNEL_ID=1001 (so we echo the raw
            // wire value), but the channelId field is a plain PER integer16
            // with min=0 — the FULL channel id. Our user channel id == userId
            // (initiator + 1001 => 1004), then the global IO channel 1003.
            int realUserId = userId + 1001;
            SendAll(s, ChannelJoinRequest(userId, realUserId));
            ExpectJoinConfirm(stream, userId, realUserId);
            SendAll(s, ChannelJoinRequest(userId, 1003));
            ExpectJoinConfirm(stream, userId, 1003);
            if (drdynvcChan >= 0) {
                SendAll(s, ChannelJoinRequest(userId, drdynvcChan));
                ExpectJoinConfirm(stream, userId, drdynvcChan);
            }
            s_drdynvcChan = drdynvcChan; s_userId = userId; s_sock = s;
            // ChannelJoin: our user channel, then the global IO channel 1007.
            // ClientInfo wrapped in an MCS SendDataRequest on global 1003.
            SendAll(s, BuildClientInfo(userId));

            // drdynvc traffic is handled opportunistically inside every read
            // loop via ReadTpktHandlingDvc (see S10 comment).

            // --- S7: Demand Active -> Confirm Active ------------------------
            // Server answers ClientInfo with Demand Active as an MCS SendData-
            // Indication (App26 = 0x68) on channel 1003; Share Control Header
            // starts after the SDI header (variable, see SdiUserDataOffset).
            // Header: totalLength(2) pduType(2 = PDU_TYPE_DEMAND_ACTIVE|0x10 =
            // 0x11) pduSource(2), then shareId(4).
            var da = ExpectServerPdu(stream, 0x01, "demand-active", 10000);
            int daOff = SdiUserDataOffset(da);
            uint shareId = (uint)(da[daOff+6] | (da[daOff+7] << 8) | (da[daOff+8] << 16) | (da[daOff+9] << 24));
            Pass("demand-active", "server shareId=0x" + shareId.ToString("x8"));

            // Confirm Active (PDU_TYPE_CONFIRM_ACTIVE|0x10 = 0x13). One
            // capability set: CAPSET_TYPE_MULTI_FRAGMENT_UPDATE (0x001A) so
            // the server's MultifragMaxRequestSize is not clamped to the
            // 16256 default (else every full-screen fastpath frame errors
            // with "exceeds the client's maximum request size").
            var caps = new List<byte>();
            AddU16(caps, 1);                   // numberCapabilities
            AddU16(caps, 0);                   // pad2Octets
            AddU16(caps, 0x001A);              // capabilitySetType
            AddU16(caps, 8);                   // lengthCapability
            AddU32(caps, 0x00FFFF08);          // MaxRequestSize
            var ca = new List<byte>();
            AddU32(ca, (int)shareId);          // shareId echoed back
            AddU16(ca, 1002);                  // originatorId = ORG_ID_SRV (1002)
            byte[] srcDesc = System.Text.Encoding.ASCII.GetBytes("MSTSC\0");
            AddU16(ca, srcDesc.Length);        // lengthSourceDescriptor
            AddU16(ca, caps.Count + 4);        // lengthCombinedCapabilities
            ca.AddRange(srcDesc);
            ca.AddRange(caps);
            SendAll(s, ShareControlPdu(userId, 0x13, ca.ToArray()));

            // --- S8: client finalization (MS-RDPBCGR 1.3.1.1) ----------------
            // Synchronize (DATA_PDU_TYPE_SYNCHRONIZE=0x1F)
            SendAll(s, ShareControlPdu(userId, 0x17, DataPdu(shareId, 0x1F,
                new byte[]{ 0x01, 0x00, 0x00, 0x00 })));        // msgType=SYNC, targetUser
            // Control PDU - Cooperate (CTRLACTION_COOPERATE=0x0004)
            SendAll(s, ShareControlPdu(userId, 0x17, DataPdu(shareId, 0x14,
                new byte[]{ 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00 })));
            // Control PDU - Request Control (CTRLACTION_REQUEST_CONTROL=0x0001)
            SendAll(s, ShareControlPdu(userId, 0x17, DataPdu(shareId, 0x14,
                new byte[]{ 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00 })));
            // Font List (DATA_PDU_TYPE_FONT_LIST=0x27): 8-byte body, skipped
            // as opaque by rdp_recv_client_font_list_pdu.
            SendAll(s, ShareControlPdu(userId, 0x17, DataPdu(shareId, 0x27,
                new byte[]{ 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x32, 0x00 })));

            // --- S9: server finalization + hold -----------------------------
            // Expect server Synchronize(0x1F), Control(0x14) x2, Font Map(0x28)
            // as it walks to CONNECTION_STATE_ACTIVE. Report, then hold.
            for (int i = 0; i < 4; i++) {
                var fin = ReadTpktHandlingDvc(stream, 8000);
                if (fin == null) Fail("finalization", "server went quiet mid-finalization");
                if (fin.Length > 19 && fin[7] == 0x68) {
                    int fo = SdiUserDataOffset(fin);
                    if (fo < 0) continue;
                    int pduType = fin[fo+2] & 0x0f;
                    int type2 = (pduType == 0x7 && fin.Length > fo + 14) ? fin[fo + 14] : -1;
                    Console.WriteLine("[probe] finalization pdu type=" + pduType + " pduType2=0x" + (type2 < 0 ? "-" : type2.ToString("x2")));
                }
            }
            Pass("finalization", "server sent finalization sequence");

            // --- S9b: input burst (opt-in via --send-input) -----------------
            // Must come AFTER finalization: FreeRDP silently drops input until
            // the server reaches CONNECTION_STATE_ACTIVE. Emit a known
            // keystroke (scancode 0x2C = 'z', press + release) so the harness
            // can assert the char actually landed in a focused guest widget.
            // This is the FIRST input assertion in the gate: it must pass
            // before any persistent-vkbd / input-thread refactor is judged
            // non-regressing. Default off (harness passes the flag once it has
            // prepared a capture sink on the guest).
            if (sendInput) {
                try {
                    Thread.Sleep(2500);  // let the sink's window realize + get focused on the headless output
                    SendScancodeKey(s, userId, shareId, 0x2C);  // 'z' scancode
                    Console.WriteLine("[probe] input sent: scancode 0x2C (z) press+release");
                } catch (Exception iex) {
                    Console.WriteLine("[probe] WARN input burst failed: " + iex.Message);
                }
            }

            // --- S10: drdynvc pump -----------------------------------------
            // Hold the session while the DVC-aware read answers the server
            // Capability Request (flips wayrdp to DRDYNVC_STATE_READY=2) and
            // then its rdpgfx Create Request. The real vmconnect client opens
            // EGFX immediately; wayrdp only starts EGFX frame flow
            // (send_frame_gfx) after this point.
            var holdEnd = Environment.TickCount + settleSec * 1000;
            while (Environment.TickCount < holdEnd) {
                var hp = ReadTpktHandlingDvc(stream, Math.Min(500, Math.Max(50, holdEnd - Environment.TickCount)));
                if (hp != null) {
                    int hch = (hp.Length > 12 && hp[7] == 0x68) ? ((hp[10] << 8) | hp[11]) : -1;
                    string tag = hp.Length > 7 ? "0x" + hp[7].ToString("x2") : "-";
                    string head = hp.Length > 7 ? BitConverter.ToString(hp, 7, Math.Min(hp.Length - 7, 24))
                        : BitConverter.ToString(hp, 0, hp.Length);
                    Console.WriteLine("[probe][dbg] hold rx: chId=" + hch + " tag=" + tag +
                        " len=" + hp.Length + " head=" + head);
                }
            }
            // Wayrdp re-queues the drdynvc capability request while
            // DRDYNVC_STATE_INITIALIZED (throttled ~1.9s). If the hold above
            // closed just before a retry landed, the gate below can miss a
            // caps request that is already in flight. Give the gate one last
            // short pump so a late (re)sent request is still answered before
            // we declare the stage failed.
            // Wayrdp re-queues the drdynvc capability request while
            // DRDYNVC_STATE_INITIALIZED (throttled ~1.9s). If the hold above
            // closed just before a retry landed, the gate below can miss a
            // caps request that is already in flight. Give the caps gate one
            // last short pump so a late (re)sent request is still answered
            // before we declare the stage failed.
            if (!s_capsDone) {
                var t0 = Environment.TickCount;
                while (!s_capsDone && Environment.TickCount - t0 < 2500) {
                    ReadTpktHandlingDvc(stream, 300);
                }
            }
            // wayrdp only emits the drdynvc Capability Request for clients that
            // negotiate the Graphics Pipeline (EGFX). This prober intentionally
            // runs VMConnect *basic* mode (no SupportGraphicsPipeline), so a
            // missing caps request is expected, not a defect. Report, don't fail;
            // frame delivery is asserted by the harness via A6/A7 instead.
            if (!s_capsDone) {
                Console.WriteLine("[probe] note: no drdynvc caps request (expected in basic mode; EGFX clients only)");
            } else {
                Pass("dvc-caps", "server requested drdynvc capabilities");
            }
            // EGFX (rdpgfx) is only created server-side when the client
            // negotiates the Graphics Pipeline. This prober intentionally
            // emulates VMConnect *basic* mode, which does NOT offer
            // SupportGraphicsPipeline, so wayrdp correctly stays on the
            // slow-path bitmap encoder and never opens rdpgfx here. Treat the
            // EGFX channel as informational in this mode; frame delivery is
            // asserted by the harness via the slow-path [gfx-lat] breadcrumbs.
            if (s_rdpgfxChanId < 0) {
                Console.WriteLine("[probe] note: no EGFX dynamic channel (basic mode does not negotiate the graphics pipeline) - slow-path frames expected");
            } else {
                Pass("dvc-rdpgfx", "accepted EGFX dynamic channel id=" + s_rdpgfxChanId);
            }
            Console.WriteLine("[probe] ALL_STAGES_PASS — vmconnect handshake replicated");
            return 0;
        } catch (Exception ex) {
            Fail("exception", ex.GetType().Name + ": " + ex.Message + " @ " + ex.StackTrace);
            return 2;
        } finally {
            if (s != IntPtr.Zero) { try { shutdown(s, SD_BOTH); } catch {} closesocket(s); }
        }
    }

    // Expect a ChannelJoinConfirm (App15 = 0x3e); skip other PDUs meanwhile.
    static void ExpectJoinConfirm(HvStream stream, int initiator, int channelId) {
        for (int i = 0; i < 6; i++) {
            var p = stream.ReadTpkt(8000);
            if (p == null) Fail("channel-join", "timeout waiting for JoinConfirm channel=" + channelId);
            if (p[7] == 0x3e) { Pass("channel-join", "joined channel=" + channelId); return; }
        }
        Fail("channel-join", "no JoinConfirm for channel=" + channelId);
    }

    // ClientInfo (MS-RDPBCGR 2.2.1.11 TS_INFO_PACKET + TS_EXTENDED_INFO_PACKET)
    // inside an MCS SendDataRequest on the global IO channel (1007).
    static byte[] BuildClientInfo(int userId) {
        byte[] userB = System.Text.Encoding.Unicode.GetBytes("wayrdp-probe\0");
        var b = new List<byte>();
        // RDP Basic Security Header: the server's Client Info reader
        // (rdp_recv_client_info) does rdp_read_security_header first and
        // requires the SEC_INFO_PKT flag (0x0040) even with no encryption.
        AddU16(b, 0x0040);       // flags = SEC_INFO_PKT
        AddU16(b, 0);            // flagsHi
        AddU32(b, 0);            // CodePage
        AddU32(b, 0x00000011);   // flags: INFO_MOUSE(0x01) | INFO_UNICODE(0x10)
        // FreeRDP 3.31 rdp_read_info_string: cbX counts the content bytes
        // EXCLUDING the mandatory NUL terminator, which is read separately.
        // userB currently includes the trailing NUL; split it out.
        int userLen = userB.Length - 2;         // exclude trailing UTF-16 NUL
        AddU16(b, 0);            // cbDomain    (empty)
        AddU16(b, userLen);      // cbUserName  (content bytes only)
        AddU16(b, 0);            // cbPassword
        AddU16(b, 0);            // cbAlternateShell
        AddU16(b, 0);            // cbWorkingDir
        b.AddRange(new byte[]{0,0});   // Domain NUL terminator
        b.AddRange(userB);             // UserName (content + its NUL)
        b.AddRange(new byte[]{0,0});   // Password NUL
        b.AddRange(new byte[]{0,0});   // AlternateShell NUL
        b.AddRange(new byte[]{0,0});   // WorkingDir NUL
        // TS_EXTENDED_INFO_PACKET
        AddU16(b, 1);                  // clientAddressFamily = AF_INET
        AddU16(b, 2);                  // cbClientAddress
        b.AddRange(new byte[]{0,0});
        AddU16(b, 2);                  // cbClientDir
        b.AddRange(new byte[]{0,0});
        b.AddRange(new byte[172]);     // clientTimeZone
        AddU32(b, 0);                  // clientSessionId
        AddU32(b, 3);                  // performanceFlags
        AddU16(b, 0);                  // cbAutoReconnectCookie
        AddU16(b, 0);                  // reserved1
        AddU16(b, 0);                  // reserved2

        // ClientInfo rides an MCS SendDataRequest on the global IO channel.
        return SendDataRequest(userId, b.ToArray());
    }
    // Wait for an MCS SendDataIndication (0x68) on channel 1003 whose Share
    // Control pduType (low nibble) matches; skip other PDUs meanwhile.
    static byte[] ExpectServerPdu(HvStream stream, int pduType, string stage, int timeoutMs) {
        for (int i = 0; i < 12; i++) {
            var p = ReadTpktHandlingDvc(stream, timeoutMs);
            if (p == null) Fail(stage, "timeout waiting for server pduType=0x0" + pduType.ToString("x"));
            if (p.Length > 7 && p[7] == 0x68) {
                int o = SdiUserDataOffset(p);
                if (o >= 0 && (p[o+2] & 0x0f) == pduType) return p;
                Console.WriteLine("[probe][dbg] skipping SDI scType=" + (o >= 0 ? "0x" + p[o+2].ToString("x2") : "bad") + " len=" + p.Length);
            } else if (p.Length > 7) {
                Console.WriteLine("[probe][dbg] skipping pdu tpkt=0x" + p[7].ToString("x2"));
            }
        }
        Fail(stage, "no server pduType=0x0" + pduType.ToString("x"));
        return null;
    }

    // MCS SendDataRequest on global channel 1003 wrapping a Share Control PDU.
    static byte[] ShareControlPdu(int userId, byte pduType, byte[] body) {
        int total = body.Length + 6;  // 6 = share control header
        var sc = new List<byte>();
        AddU16(sc, total);            // totalLength
        AddU16(sc, pduType);          // pduType|pduVersion(1)
        AddU16(sc, userId + 1001);    // pduSource = our channel id
        sc.AddRange(body);
        return SendDataRequest(userId, sc.ToArray());
    }

    // Share Data Header for a data PDU (pduType2 on global channel).
    static byte[] DataPdu(uint shareId, byte pduType2, byte[] body) {
        var d = new List<byte>();
        AddU32(d, (int)shareId);      // shareId
        d.Add(0);                     // pad1
        d.Add(0);                     // streamId = STREAM_LOW
        AddU16(d, body.Length);       // uncompressedLength
        d.Add(pduType2);              // pduType2
        d.Add(0);                     // compressedType
        AddU16(d, 0);                 // compressedLength
        d.AddRange(body);
        return d.ToArray();
    }

    // MCS SendDataRequest (App25 = 0x64) on the global IO channel 1003.
    static byte[] SendDataRequest(int userId, byte[] userData) {
        return SendDataRequestChan(userId, 1003, userData);
    }

    static byte[] SendDataRequestChan(int userId, int channelId, byte[] userData) {
        var sdr = new List<byte>();
        sdr.Add(0x64);
        sdr.Add((byte)(userId>>8)); sdr.Add((byte)userId);
        sdr.Add((byte)(channelId>>8)); sdr.Add((byte)(channelId & 0xff));
        sdr.Add(0x70);                // dataPriority + segmentation(begin|end)
        if (userData.Length < 0x80) {
            sdr.Add((byte)userData.Length);
        } else {
            sdr.Add((byte)(0x80 | (userData.Length >> 8))); sdr.Add((byte)(userData.Length & 0xff));
        }
        sdr.AddRange(userData);
        return Tpkt(sdr.ToArray());
    }

    static void AddU16(List<byte> b, int v) { b.Add((byte)v); b.Add((byte)(v>>8)); }
    static void AddU32(List<byte> b, int v) { b.Add((byte)(v&0xff)); b.Add((byte)((v>>8)&0xff)); b.Add((byte)((v>>16)&0xff)); b.Add((byte)((v>>24)&0xff)); }

    // --- drdynvc (MS-RDPEDYC 2.2.x) ----------------------------------------
    // Static channel userData framing: CHANNEL_PDU length(4 LE, total
    // uncompressed data length across chunks) + flags(4 LE;
    // FIRST=0x01|LAST=0x02). drdynvc body: hdr byte Cmd<<4|Sp<<2|cbChId,
    // pad(1), version(2 LE).
    const int DVC_CAPS_REQUEST = 0x50;    // Cmd=5,cbChId=0 (server->client)
    const int DVC_CAPS_RESPONSE = 0x50;   // same opcode; MSTSC sends 0x005c
    const int DVC_CREATE_REQUEST = 0x10;  // Cmd=1,cbChId=0
    const int DVC_CREATE_RESPONSE = 0x10;
    const string RDPEGFX_CHANNEL = "Microsoft::Windows::RDS::Graphics";

    static byte[] DvcChannelPdu(byte[] dvcBody) {
        var p = new List<byte>();
        AddU32(p, dvcBody.Length);        // total length (single chunk)
        AddU32(p, 0x03);                  // CHANNEL_FLAG_FIRST|LAST
        p.AddRange(dvcBody);
        return p.ToArray();
    }

    // --- shared DVC-aware read ---------------------------------------------
    // After the drdynvc static channel is joined, the wayrdp mainloop calls
    // VCM open, which queues the server Capability Request. It can interleave
    // with ANY later phase (demand-active, finalization, frames), so every
    // read after the joins must route through this wrapper: drdynvc SDIs are
    // answered inline (caps response / DVC create response for the EGFX
    // channel) and skipped; anything else is returned to the caller.
    static int s_drdynvcChan = -1;
    static int s_userId = -1;
    static IntPtr s_sock = IntPtr.Zero;
    static bool s_capsDone = false;
    static int s_rdpgfxChanId = -1;
    static bool sendInput = false;

    // Input burst on the slow path (Share Control, pduType2=0x1C = Input).
    // TS_INPUT_PDU_DATA (MS-RDPBCGR 2.2.8.1.1.3.1) as ACTUALLY parsed by
    // FreeRDP 3.32.1 libfreerdp/core/input.c input_recv()/input_recv_event()
    // (the wire layout FreeRDP implements, verified against source):
    //   shareId(4) pad(1) streamId(1) uncompressedLength(2) pduType2(1)
    //   compressedType(1) compressedLength(2)  <- produced by DataPdu()
    //   numEvents(2)  pad(2)               <- PDU header, no action field
    // then per event (TS_INPUT_EVENT_SCANCODE, 6-byte body after header):
    //   eventTime(4)  messageType(2)=0x0004 INPUT_EVENT_SCANCODE
    //   keyboardFlags(2)  (KBDFLAGS_RELEASE=0x8000 on release; 0 on press;
    //     KBDFLAGS_EXTENDED=0x0100 — do not use, wayrdp ORs it into the code)
    //   keyCode(2) little-endian scancode  pad(2)
    // wayrdp's peer_input_keyboard handles the scancode path (the unicode path
    // is a deliberate no-op in the backend), so a scancode 'z' (0x2C) reaches
    // the focused widget and A2b can grep it in the sink log.
    static void SendScancodeKey(IntPtr s, int userId, uint shareId, ushort keyCode) {
        var body = new List<byte>();
        AddU16(body, 2);               // numEvents = 2 (press + release)
        AddU16(body, 0x0000);          // pad2Octets
        // press
        AddU32(body, 0);               // eventTime (server-ignored)
        AddU16(body, 0x0004);          // messageType = INPUT_EVENT_SCANCODE
        AddU16(body, 0x0000);          // keyboardFlags: press (KBDFLAGS_DOWN implied)
        AddU16(body, keyCode);         // keyCode (2 bytes LE)
        AddU16(body, 0x0000);          // pad
        // release
        AddU32(body, 0);               // eventTime
        AddU16(body, 0x0004);          // messageType = INPUT_EVENT_SCANCODE
        AddU16(body, 0x8000);          // keyboardFlags: KBDFLAGS_RELEASE
        AddU16(body, keyCode);         // keyCode
        AddU16(body, 0x0000);          // pad
        SendAll(s, ShareControlPdu(userId, 0x17, DataPdu(shareId, 0x1C, body.ToArray())));
    }

    static byte[] ReadTpktHandlingDvc(HvStream stream, int timeoutMs) {
        var deadline = Environment.TickCount + timeoutMs;
        while (true) {
            int remain = Math.Max(50, deadline - Environment.TickCount);
            var p = stream.ReadTpkt(remain);
            if (p == null) return null;
            if (p.Length <= 13 || p[7] != 0x68) return p;
            int chId = (p[10] << 8) | p[11];
            if (chId != s_drdynvcChan || s_drdynvcChan < 0) return p;
            int o = SdiUserDataOffset(p);
            // Need the 8-byte CHANNEL_PDU header + at least 1 payload byte
            // (the drdynvc cmd) strictly inside the packet.
            if (o < 0 || p.Length <= 9 + o) continue;
            int cmd = p[o + 8];           // skip CHANNEL_PDU header (8B)
            Console.WriteLine("[probe][dbg] drdynvc pdu cmd=0x" + cmd.ToString("x2") + " len=" + p.Length);
            if (cmd == DVC_CAPS_REQUEST) {
                SendAll(s_sock, SendDataRequestChan(s_userId, s_drdynvcChan,
                    DvcChannelPdu(new byte[]{ DVC_CAPS_RESPONSE, 0x00, 0x01, 0x00 })));
                if (!s_capsDone) { s_capsDone = true; Pass("dvc-caps", "answered drdynvc capability request"); }
            } else if ((cmd & 0xf0) == DVC_CREATE_REQUEST) {
                int cb = cmd & 0x03;
                int cbLen = cb == 0 ? 1 : cb == 1 ? 2 : 4;
                int id = 0;
                for (int k = 0; k < cbLen && o + 9 + k < p.Length; k++)
                    id |= p[o + 9 + k] << (8 * k);
                var nameBytes = new List<byte>();
                for (int k = o + 9 + cbLen; k < p.Length && p[k] != 0; k++) nameBytes.Add(p[k]);
                string chName = System.Text.Encoding.ASCII.GetString(nameBytes.ToArray());
                Console.WriteLine("[probe][dbg] DVC create: id=" + id + " name=" + chName);
                if (chName == RDPEGFX_CHANNEL) {
                    var cresp = new List<byte>();
                    cresp.Add((byte)(DVC_CREATE_RESPONSE | cb));
                    for (int k = 0; k < cbLen; k++) cresp.Add((byte)(id >> (8 * k)));
                    AddU32(cresp, 0);      // CreationStatus = success
                    SendAll(s_sock, SendDataRequestChan(s_userId, s_drdynvcChan, DvcChannelPdu(cresp.ToArray())));
                    s_rdpgfxChanId = id;
                    Pass("dvc-rdpgfx", "accepted EGFX dynamic channel id=" + id);
                }
            }
            // drdynvc packet handled; keep reading for the caller's PDU
        }
    }
}
