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

        Thread.detachNewThread { [weak self] in
            while true {
                let client = accept(fd, nil, nil)
                if client < 0 { return }
                guard let self, !self.isStopped else {
                    close(client)
                    return
                }
                self.serve(client)
            }
        }
    }

    /// Every request line + headers this stub received, in order.
    var requests: [String] {
        lock.lock()
        defer { lock.unlock() }
        return seen
    }

    func stop() {
        lock.lock()
        let already = stopped
        stopped = true
        lock.unlock()
        if !already { close(listenFD) }
    }

    private var isStopped: Bool {
        lock.lock()
        defer { lock.unlock() }
        return stopped
    }

    private func serve(_ client: Int32) {
        var request = ""
        var buf = [UInt8](repeating: 0, count: 4096)
        // Headers first; then whatever body the Content-Length announces (the
        // provider always sends one, and reading it keeps the peer from seeing a
        // reset before it finishes writing).
        while !request.contains("\r\n\r\n") {
            let n = read(client, &buf, buf.count)
            if n <= 0 { break }
            request += String(decoding: buf[0..<n], as: UTF8.self)
        }
        if let range = request.range(of: "Content-Length:", options: .caseInsensitive) {
            let tail = request[range.upperBound...]
            let value = tail.prefix { $0 != "\r" && $0 != "\n" }.trimmingCharacters(in: .whitespaces)
            let want = Int(value) ?? 0
            var have = request.components(separatedBy: "\r\n\r\n").dropFirst()
                .joined(separator: "\r\n\r\n").utf8.count
            while have < want {
                let n = read(client, &buf, buf.count)
                if n <= 0 { break }
                request += String(decoding: buf[0..<n], as: UTF8.self)
                have += n
            }
        }

        lock.lock()
        seen.append(request)
        let reply = pending.isEmpty ? Reply(status: 500, body: "no scripted reply") : pending.removeFirst()
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
