using System;
using System.Collections.Generic;
using System.Net;
using System.Net.Sockets;
using System.Runtime.InteropServices;
using System.Threading;

// vsfwd - TCP<->Hyper-V socket (AF_HYPERV) forwarder for vmconnect-path
// testing with ordinary TCP clients (xfreerdp3).
//
// Usage: vsfwd.exe <vmGuid> <serviceGuid> <tcpPort>
//
// The hv socket lives entirely on ONE thread: blocking recv with a short
// SO_RCVTIMEO, then a bounded flush of the pending client->server queue.
// (Two dedicated pump threads on one raw AF_HYPERV handle - one send, one
// recv, concurrently - wedged after the first exchange; the up-pump's
// second send blocked forever while the down-pump sat in recv.)

public class VsFwd {
    [DllImport("ws2_32.dll", SetLastError=true)]
    static extern IntPtr WSASocketW(int af, int type, int protocol, IntPtr lpProtocolInfo, uint g, uint dwFlags);
    [DllImport("ws2_32.dll", SetLastError=true)]
    static extern int WSAConnect(IntPtr s, byte[] name, int namelen, IntPtr a, IntPtr b, IntPtr c, IntPtr d);
    [DllImport("ws2_32.dll")] static extern int closesocket(IntPtr s);
    [DllImport("ws2_32.dll")] static extern int send(IntPtr s, byte[] buf, int len, int flags);
    [DllImport("ws2_32.dll")] static extern int recv(IntPtr s, byte[] buf, int len, int flags);
    [DllImport("ws2_32.dll")] static extern int shutdown(IntPtr s, int how);
    [DllImport("ws2_32.dll", SetLastError=true)] static extern int setsockopt(IntPtr s, int level, int name, byte[] val, int len);
    [DllImport("ws2_32.dll", SetLastError=true)] static extern int WSAGetLastError();

    const int AF_HYPERV = 34, SOCK_STREAM = 1, HV_PROTOCOL_RAW = 1;
    const int SD_SEND = 1;
    const int SOL_SOCKET = 0xffff, SO_RCVTIMEO = 0x1006;

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

    public static void Main(string[] args) {
        using (var t = new Socket(AddressFamily.InterNetwork, SocketType.Stream, ProtocolType.Tcp)) {}
        Guid vmId = new Guid(args[0]);
        Guid svc = new Guid(args[1]);
        int port = int.Parse(args[2]);
        // Loopback-only: this forwards the vsock path, which uses the legacy
        // RDP security layer (no encryption/NLA). Binding to every interface
        // would expose that to the whole LAN, defeating the "TCP is NLA-only"
        // rule for any device on the network.
        var listen = new TcpListener(IPAddress.Loopback, port);
        listen.Start();
        Console.WriteLine("[vsfwd] listening 127.0.0.1:" + port + " -> " + vmId + " " + svc);
        while (true) {
            var tcp = listen.AcceptSocket();
            tcp.NoDelay = true;
            tcp.Blocking = false;
            Console.WriteLine("[vsfwd] tcp " + tcp.RemoteEndPoint);
            ThreadPool.QueueUserWorkItem(_ => {
                IntPtr vs = IntPtr.Zero;
                try {
                    vs = ConnectHv(vmId, svc);
                    Console.WriteLine("[vsfwd] vsock up");
                    Session(tcp, vs);
                } catch (Exception ex) {
                    Console.WriteLine("[vsfwd] " + ex.Message);
                } finally {
                    if (vs != IntPtr.Zero) closesocket(vs);
                    try { tcp.Close(); } catch {}
                    Console.WriteLine("[vsfwd] session closed");
                }
            });
        }
    }

    // Upstream queue: tcp reader thread appends; hv owner thread drains.
    class UpQ { public readonly object L = new object(); public List<byte[]> Chunks = new List<byte[]>(8); public long Bytes; public bool Eof; }

    static void Session(Socket tcp, IntPtr vs) {
        var q = new UpQ();
        var hvBuf = new byte[1 << 16];
        var tcpBuf = new byte[1 << 16];
        long up = 0, down = 0;
        bool tcpEofSignaled = false;

        // TCP reader: appends client bytes to the queue.
        var reader = new Thread(() => {
            try {
                while (true) {
                    int n;
                    try { n = tcp.Receive(tcpBuf); }
                    catch (SocketException se) {
                        if (se.SocketErrorCode == SocketError.WouldBlock) { Thread.Sleep(1); continue; }
                        break;
                    }
                    if (n == 0) break;
                    var c = new byte[n]; Buffer.BlockCopy(tcpBuf, 0, c, 0, n);
                    lock (q.L) { q.Chunks.Add(c); q.Bytes += n; Monitor.Pulse(q.L); }
                }
            } catch {}
            lock (q.L) { q.Eof = true; Monitor.Pulse(q.L); }
        });
        reader.IsBackground = true;
        reader.Start();

        // hv owner: drain queue (send), then recv with 20ms timeout, relay
        // server bytes to the TCP client.
        bool dead = false;
        while (!dead) {
            // 1) drain upstream
            while (true) {
                byte[] chunk = null;
                lock (q.L) { if (q.Chunks.Count > 0) chunk = q.Chunks[0]; }
                if (chunk == null) break;
                int off = 0;
                while (off < chunk.Length) {
                    int w = send(vs, chunk, chunk.Length - off, 0);
                    if (w <= 0) { dead = true; break; }
                    off += w; up += w;
                }
                if (dead) break;
                lock (q.L) { q.Chunks.RemoveAt(0); }
            }
            if (dead) break;
            // 2) upstream EOF -> half-close hv send side, keep draining
            lock (q.L) {
                if (q.Eof && q.Chunks.Count == 0 && !tcpEofSignaled) {
                    tcpEofSignaled = true;
                    shutdown(vs, SD_SEND);
                }
            }
            // 3) recv from hv (20ms timeout -> WSAETIMEDOUT)
            int n = recv(vs, hvBuf, hvBuf.Length, 0);
            if (n > 0) {
                int s = 0;
                while (s < n && !dead) {
                    try { s += tcp.Send(hvBuf, s, n - s, SocketFlags.None); }
                    catch (SocketException se) {
                        if (se.SocketErrorCode == SocketError.WouldBlock) { Thread.Sleep(1); continue; }
                        dead = true;
                    }
                    catch { dead = true; }
                }
                down += n;
                if (down < 4096 || (down & 0xFFFFF) < n) Console.WriteLine("[vsfwd] down total=" + down);
            } else if (n == 0) {
                break; // server closed
            } else {
                int e = WSAGetLastError();
                if (e != 10060) { Console.WriteLine("[vsfwd] hv recv err " + e); break; }
            }
            // 4) exit when both directions are finished
            lock (q.L) { if (q.Eof && q.Chunks.Count == 0 && tcpEofSignaled) { /* wait for server close */ } }
        }
        Console.WriteLine("[vsfwd] session end up=" + up + " down=" + down);
    }
}