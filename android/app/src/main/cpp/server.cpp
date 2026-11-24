#include "rdcore.h"

#include <errno.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <chrono>
#include <cstddef>
#include <cstring>

namespace rd {

namespace {

constexpr size_t kMaxQueued = 120;

void put_u16(std::vector<uint8_t>& v, uint16_t x) {
    v.push_back(x & 0xff);
    v.push_back(x >> 8);
}
void put_u32(std::vector<uint8_t>& v, uint32_t x) {
    for (int i = 0; i < 4; i++) v.push_back((x >> (8 * i)) & 0xff);
}
uint32_t get_u32(const uint8_t* p) { return p[0] | (p[1] << 8) | (p[2] << 16) | (uint32_t(p[3]) << 24); }
int32_t get_i32(const uint8_t* p) { return static_cast<int32_t>(get_u32(p)); }

bool recv_all(int fd, uint8_t* p, size_t n) {
    while (n > 0) {
        ssize_t r = ::recv(fd, p, n, 0);
        if (r > 0) {
            p += r;
            n -= static_cast<size_t>(r);
        } else if (r == 0) {
            return false;
        } else if (errno != EINTR) {
            return false;
        }
    }
    return true;
}

}  // namespace

// ---- Client ---------------------------------------------------------------------------------

Client::Client(int fd, InputSink* sink, std::function<void()> request_keyframe)
    : fd_(fd), sink_(sink), request_keyframe_(std::move(request_keyframe)) {}

Client::~Client() { ::close(fd_); }

void Client::start() {
    auto self = shared_from_this();
    std::thread([self] { self->writer_loop(); }).detach();
    std::thread([self] { self->reader_loop(); }).detach();
}

void Client::close() {
    if (alive_.exchange(false)) {
        // shutdown() unblocks send/recv in the worker threads; the fd is closed in the destructor.
        ::shutdown(fd_, SHUT_RDWR);
        std::lock_guard<std::mutex> l(mu_);
        queue_.clear();
    }
    cv_.notify_all();
}

void Client::enqueue(Msg&& m) {
    const bool video = m.type == T_VIDEO_FRAME;
    const bool key = (m.flags & kFlagKeyframe) != 0;
    bool need_keyframe = false;
    {
        std::lock_guard<std::mutex> l(mu_);
        if (!alive_) return;
        if (video && wait_key_ && !key) return;
        if (queue_.size() >= kMaxQueued) {
            if (video) {
                wait_key_ = true;
                need_keyframe = true;
            }
        } else {
            if (video && key) wait_key_ = false;
            queue_.push_back(std::move(m));
        }
    }
    if (need_keyframe && request_keyframe_) request_keyframe_();
    cv_.notify_one();
}

bool Client::send_all(const uint8_t* p, size_t n) {
    while (n > 0) {
        ssize_t w = ::send(fd_, p, n, MSG_NOSIGNAL);
        if (w > 0) {
            p += w;
            n -= static_cast<size_t>(w);
        } else if (w < 0 && errno == EINTR) {
            continue;
        } else {
            return false;
        }
    }
    return true;
}

void Client::writer_loop() {
    for (;;) {
        Msg m;
        {
            std::unique_lock<std::mutex> l(mu_);
            cv_.wait(l, [this] { return !queue_.empty() || !alive_; });
            if (!alive_) break;
            m = std::move(queue_.front());
            queue_.pop_front();
        }
        uint8_t h[kHeaderLen];
        h[0] = m.type;
        h[1] = m.flags;
        const uint32_t len = static_cast<uint32_t>(m.payload.size());
        for (int i = 0; i < 4; i++) h[2 + i] = (len >> (8 * i)) & 0xff;
        for (int i = 0; i < 8; i++) h[6 + i] = (m.pts >> (8 * i)) & 0xff;
        if (!send_all(h, sizeof h) || !send_all(m.payload.data(), m.payload.size())) break;
    }
    close();
}

void Client::reader_loop() {
    uint8_t h[kHeaderLen];
    std::vector<uint8_t> payload;
    while (alive_ && recv_all(fd_, h, sizeof h)) {
        const uint8_t type = h[0];
        const uint32_t len = get_u32(h + 2);
        if (len > kMaxInboundPayload) break;
        payload.resize(len);
        if (len && !recv_all(fd_, payload.data(), len)) break;
        const uint8_t* p = payload.data();
        switch (type) {
            case T_REQUEST_KEYFRAME:
                if (request_keyframe_) request_keyframe_();
                break;
            case T_TOUCH:  // action u8, pointer u8, x y fw fh u32
                if (len >= 18) sink_->touch(p[0], get_i32(p + 2), get_i32(p + 6), get_i32(p + 10), get_i32(p + 14));
                break;
            case T_SCROLL:  // x y fw fh u32, dx dy i32
                if (len >= 24) sink_->scroll(get_i32(p), get_i32(p + 4), get_i32(p + 8), get_i32(p + 12), get_i32(p + 16), get_i32(p + 20));
                break;
            case T_NAV:
                if (len >= 1) sink_->nav(p[0]);
                break;
            case T_TEXT:
                sink_->text(p, len);
                break;
            case T_KEY:  // down u8, keycode i32, meta i32
                if (len >= 9) sink_->key(p[0] != 0, get_i32(p + 1));
                break;
            default:
                break;
        }
    }
    close();
}

// ---- Server ---------------------------------------------------------------------------------

Server::Server(InputSink* sink, uint32_t version_code, uint32_t sdk, std::string model)
    : sink_(sink), version_code_(version_code), sdk_(sdk), model_(std::move(model)) {}

Server::~Server() { stop(); }

void Server::set_keyframe_requester(std::function<void()> f) {
    std::lock_guard<std::mutex> l(mu_);
    request_keyframe_ = std::move(f);
}

bool Server::start(const char* name) {
    listen_fd_ = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (listen_fd_ < 0) {
        LOGE("socket: %s", strerror(errno));
        return false;
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    const size_t n = strlen(name);
    if (n + 1 > sizeof(addr.sun_path)) return false;
    addr.sun_path[0] = '\0';  // abstract namespace
    memcpy(addr.sun_path + 1, name, n);
    const socklen_t alen = static_cast<socklen_t>(offsetof(sockaddr_un, sun_path) + 1 + n);
    if (::bind(listen_fd_, reinterpret_cast<sockaddr*>(&addr), alen) != 0 || ::listen(listen_fd_, 1) != 0) {
        LOGE("bind/listen: %s", strerror(errno));
        ::close(listen_fd_);
        listen_fd_ = -1;
        return false;
    }
    running_ = true;
    accept_thread_ = std::thread([this] { accept_loop(); });
    status_thread_ = std::thread([this] { status_loop(); });
    LOGI("listening on @%s", name);
    return true;
}

void Server::stop() {
    if (!running_.exchange(false)) return;
    if (accept_thread_.joinable()) accept_thread_.join();
    if (status_thread_.joinable()) status_thread_.join();
    if (listen_fd_ >= 0) ::close(listen_fd_);
    listen_fd_ = -1;
    std::shared_ptr<Client> c;
    {
        std::lock_guard<std::mutex> l(mu_);
        c = std::move(client_);
    }
    if (c) c->close();
}

std::shared_ptr<Client> Server::current() {
    std::lock_guard<std::mutex> l(mu_);
    return client_;
}

void Server::accept_loop() {
    while (running_) {
        pollfd pfd{listen_fd_, POLLIN, 0};
        const int pr = ::poll(&pfd, 1, 200);
        if (pr <= 0) continue;
        const int fd = ::accept4(listen_fd_, nullptr, nullptr, SOCK_CLOEXEC);
        if (fd < 0) continue;

        std::function<void()> kf;
        {
            std::lock_guard<std::mutex> l(mu_);
            kf = request_keyframe_;
        }
        auto client = std::make_shared<Client>(fd, sink_, kf);
        std::shared_ptr<Client> old;
        std::vector<uint8_t> cfg;
        uint32_t rate;
        uint8_t ch;
        {
            std::lock_guard<std::mutex> l(mu_);
            old = std::move(client_);
            client_ = client;
            cfg = video_config_;
            rate = audio_rate_;
            ch = audio_channels_;
        }
        if (old) old->close();
        client->start();

        // Hello, then whatever the host needs to start decoding immediately.
        Msg hello;
        hello.type = T_HELLO;
        put_u16(hello.payload, kProtocolVersion);
        put_u32(hello.payload, version_code_);
        put_u32(hello.payload, sdk_);
        hello.payload.insert(hello.payload.end(), model_.begin(), model_.end());
        client->enqueue(std::move(hello));
        if (!cfg.empty()) {
            Msg m;
            m.type = T_VIDEO_CONFIG;
            m.payload = std::move(cfg);
            client->enqueue(std::move(m));
        }
        if (rate && audio_status_ == AUDIO_STREAMING) {
            Msg m;
            m.type = T_AUDIO_CONFIG;
            put_u32(m.payload, rate);
            m.payload.push_back(ch);
            client->enqueue(std::move(m));
        }
        send_status(*client);
        if (kf) kf();
    }
}

void Server::send_status(Client& c) {
    Msg m;
    m.type = T_STATUS;
    m.payload = {audio_status_.load(), static_cast<uint8_t>(sink_->input_ready() ? 1 : 0)};
    c.enqueue(std::move(m));
}

void Server::status_loop() {
    // Periodic status so the host learns when the accessibility service gets enabled.
    int ticks = 0;
    while (running_) {
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
        if (++ticks < 20) continue;
        ticks = 0;
        if (auto c = current()) send_status(*c);
    }
}

void Server::send_video_config(uint32_t w, uint32_t h, uint8_t rotation, const uint8_t* cfg, size_t n) {
    std::vector<uint8_t> payload;
    payload.reserve(9 + n);
    put_u32(payload, w);
    put_u32(payload, h);
    payload.push_back(rotation);
    payload.insert(payload.end(), cfg, cfg + n);
    {
        std::lock_guard<std::mutex> l(mu_);
        video_config_ = payload;
    }
    if (auto c = current()) {
        Msg m;
        m.type = T_VIDEO_CONFIG;
        m.payload = std::move(payload);
        c->enqueue(std::move(m));
    }
}

void Server::send_video_frame(const uint8_t* cfg, size_t cfg_n, const uint8_t* data, size_t n, bool key, uint64_t pts_us) {
    auto c = current();
    if (!c) return;
    Msg m;
    m.type = T_VIDEO_FRAME;
    m.flags = key ? kFlagKeyframe : 0;
    m.pts = pts_us;
    m.payload.reserve(cfg_n + n);  // single allocation; the only copy of the encoded frame
    if (cfg_n) m.payload.insert(m.payload.end(), cfg, cfg + cfg_n);
    m.payload.insert(m.payload.end(), data, data + n);
    c->enqueue(std::move(m));
}

void Server::set_audio(uint32_t rate, uint8_t channels, uint8_t status) {
    {
        std::lock_guard<std::mutex> l(mu_);
        audio_rate_ = rate;
        audio_channels_ = channels;
    }
    audio_status_ = status;
    if (status == AUDIO_STREAMING) {
        if (auto c = current()) {
            Msg m;
            m.type = T_AUDIO_CONFIG;
            put_u32(m.payload, rate);
            m.payload.push_back(channels);
            c->enqueue(std::move(m));
        }
    }
}

void Server::send_audio(const uint8_t* pcm, size_t n, uint64_t pts_us) {
    auto c = current();
    if (!c) return;
    Msg m;
    m.type = T_AUDIO_DATA;
    m.pts = pts_us;
    m.payload.assign(pcm, pcm + n);
    c->enqueue(std::move(m));
}

}  // namespace rd
