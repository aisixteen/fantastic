// A one-file HTTP stub for the provider wire tests.
//
// The providers' POLICY — which status codes retry, what the failure text says,
// which URL the record's `endpoint` builds — is only observable from the other
// end of a socket. python's twins get there with an httpx transport seam; swift
// has no such seam on `AsyncHTTPClient`, so this binds a real listener on
// 127.0.0.1 and serves scripted responses. Nothing leaves the loopback
// interface and nothing here needs a key.
//
// Each connection is served once and closed (`Connection: close`), so a retry
// opens a new one and `requests` reads as a list of attempts.

import Foundation

#if canImport(Darwin)
    import Darwin
#else
    import Glibc
#endif

final class StubHTTP: @unchecked Sendable {
    /// One scripted response.
    struct Reply {
        let status: Int
        let headers: [(String, String)]
        let body: String

        init(status: Int = 200, headers: [(String, String)] = [], body: String = "") {
            self.status = status
            self.headers = headers
            self.body = body
        }

        /// A finished Anthropic/OpenAI-style SSE body.
        static func sse(_ body: String) -> Reply {
            Reply(status: 200, headers: [("Content-Type", "text/event-stream")], body: body)
        }
    }

    let port: Int
    private let listenFD: Int32
    /// The pipe `stop()` writes to wake the accept thread. See `stop()`.
    private let wakeRead: Int32
    private let wakeWrite: Int32
    private let lock = NSLock()
    private var pending: [Reply]
    private var seen: [String] = []
    private var stopped = false

    init(_ replies: [Reply]) {
        pending = replies
        listenFD = socket(AF_INET, SOCK_STREAM, 0)
        precondition(listenFD >= 0, "socket() failed")
        var yes: Int32 = 1
        setsockopt(listenFD, SOL_SOCKET, SO_REUSEADDR, &yes, socklen_t(MemoryLayout<Int32>.size))
        let fd = listenFD
        var addr = sockaddr_in()
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = 0  // the OS picks a free port
        addr.sin_addr.s_addr = inet_addr("127.0.0.1")
        let bound = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        precondition(bound == 0, "bind() failed")
        precondition(listen(fd, 8) == 0, "listen() failed")
        var boundAddr = sockaddr_in()
        var len = socklen_t(MemoryLayout<sockaddr_in>.size)
        _ = withUnsafeMutablePointer(to: &boundAddr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                getsockname(fd, $0, &len)
            }
        }
        port = Int(UInt16(bigEndian: boundAddr.sin_port))
        var wake: [Int32] = [0, 0]
        precondition(pipe(&wake) == 0, "pipe() failed")
        wakeRead = wake[0]
        wakeWrite = wake[1]

        // THE ACCEPT THREAD OWNS `fd` (and the wake pipe's read end): it is the
        // only one that uses it and the only one that closes it, AFTER its last
        // `accept`. `stop()` used to `close(listenFD)` from the test's thread while
        // this loop was still accepting on that NUMBER: the OS handed the number
        // to the next test's `socket()`, this stale loop accepted THAT stub's
        // connection, saw itself stopped and closed it — the other test's
        // provider read a closed connection as "refused" (measured: the vLLM
        // suite failed 11 of 25 runs, a different test each time).
        let wr = wakeRead
        Thread.detachNewThread { [weak self] in
            defer {
                close(fd)
                close(wr)
            }
            var fds = [
                pollfd(fd: fd, events: Int16(POLLIN), revents: 0),
                pollfd(fd: wr, events: Int16(POLLIN), revents: 0),
            ]
            while true {
                fds[0].revents = 0
                fds[1].revents = 0
                if poll(&fds, 2, -1) < 0 {
                    if errno == EINTR { continue }
                    return
                }
                if fds[1].revents != 0 { return }  // stop() asked
                guard fds[0].revents & Int16(POLLIN) != 0 else { continue }
                let client = accept(fd, nil, nil)
                if client < 0 {
                    if errno == EINTR || errno == ECONNABORTED { continue }
                    return
                }
                guard let self else {
                    close(client)
                    return
                }
                self.serve(client)
            }
        }
    }

    deinit { stop() }

    /// Every request line + headers this stub received, in order.
    var requests: [String] {
        lock.lock()
        defer { lock.unlock() }
        return seen
    }

    /// Wake the accept thread and let IT close the listening socket — never
    /// close an fd another thread may still be using (see `init`). Closing the
    /// pipe's write end makes the read end readable; only `stop()` touches it.
    func stop() {
        lock.lock()
        let already = stopped
        stopped = true
        lock.unlock()
        if !already { close(wakeWrite) }
    }

    private func serve(_ client: Int32) {
        // BYTES, not a String: `\r\n` is ONE Character to swift, so a string
        // scan for the header terminator across two reads is a trap.
        var raw: [UInt8] = []
        var buf = [UInt8](repeating: 0, count: 4096)
        let terminator: [UInt8] = [13, 10, 13, 10]
        func readMore() -> Bool {
            while true {
                let n = read(client, &buf, buf.count)
                if n > 0 {
                    raw += buf[0..<n]
                    return true
                }
                if n < 0 && errno == EINTR { continue }
                return false
            }
        }
        func headerEnd() -> Int? {
            guard raw.count >= 4 else { return nil }
            for i in 0...(raw.count - 4) where Array(raw[i..<i + 4]) == terminator {
                return i + 4
            }
            return nil
        }
        // Headers first; then whatever body the Content-Length announces (the
        // provider always sends one, and reading it keeps the peer from seeing a
        // reset before it finishes writing).
        var end = headerEnd()
        while end == nil, readMore() { end = headerEnd() }
        if let end {
            let head = String(decoding: raw[0..<end], as: UTF8.self).lowercased()
            var want = 0
            for line in head.components(separatedBy: "\r\n")
            where line.hasPrefix("content-length:") {
                want =
                    Int(
                        line.dropFirst("content-length:".count).trimmingCharacters(in: .whitespaces)
                    ) ?? 0
            }
            while raw.count - end < want, readMore() {}
        }
        let request = String(decoding: raw, as: UTF8.self)

        lock.lock()
        seen.append(request)
        let reply =
            pending.isEmpty ? Reply(status: 500, body: "no scripted reply") : pending.removeFirst()
        lock.unlock()

        var head = "HTTP/1.1 \(reply.status) \(Self.reason(reply.status))\r\n"
        for (k, v) in reply.headers { head += "\(k): \(v)\r\n" }
        head += "Content-Length: \(reply.body.utf8.count)\r\n"
        head += "Connection: close\r\n\r\n"
        let out = Array((head + reply.body).utf8)
        out.withUnsafeBufferPointer { p in
            var off = 0
            while off < p.count {
                let n = write(client, p.baseAddress! + off, p.count - off)
                if n <= 0 { break }
                off += n
            }
        }
        close(client)
    }

    private static func reason(_ code: Int) -> String {
        switch code {
        case 200: return "OK"
        case 400: return "Bad Request"
        case 429: return "Too Many Requests"
        case 500: return "Internal Server Error"
        case 503: return "Service Unavailable"
        default: return "Status"
        }
    }
}
