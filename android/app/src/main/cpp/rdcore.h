// Native data plane of the Remote Display companion.
// Wire format (little-endian), see crates/protocol/src/lib.rs:
//   | type u8 | flags u8 | len u32 | pts_us u64 | payload[len] |
#pragma once

#include <android/log.h>
#include <android/native_window.h>
#include <media/NdkMediaCodec.h>
#include <media/NdkMediaFormat.h>

#include <atomic>
#include <condition_variable>
#include <cstdint>
#include <deque>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#define RD_TAG "rdcore"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, RD_TAG, __VA_ARGS__)
#define LOGW(...) __android_log_print(ANDROID_LOG_WARN, RD_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, RD_TAG, __VA_ARGS__)

namespace rd {

constexpr uint16_t kProtocolVersion = 1;
constexpr uint8_t kFlagKeyframe = 1;
constexpr size_t kHeaderLen = 14;
constexpr size_t kMaxInboundPayload = 1u << 20;

enum MsgType : uint8_t {
    T_HELLO = 0x01,
    T_VIDEO_CONFIG = 0x02,
    T_VIDEO_FRAME = 0x03,
    T_AUDIO_CONFIG = 0x04,
    T_AUDIO_DATA = 0x05,
    T_STATUS = 0x06,
    T_TOUCH = 0x10,
    T_KEY = 0x11,
    T_SCROLL = 0x12,
    T_TEXT = 0x13,
    T_REQUEST_KEYFRAME = 0x14,
    T_NAV = 0x15,
};

enum AudioStatus : uint8_t { AUDIO_UNKNOWN = 0, AUDIO_STREAMING = 1, AUDIO_UNSUPPORTED = 2, AUDIO_DENIED = 3, AUDIO_FAILED = 4 };

struct Msg {
    uint8_t type = 0;
    uint8_t flags = 0;
    uint64_t pts = 0;
    std::vector<uint8_t> payload;
};

// Implemented in jni.cpp: forwards host input to the Java accessibility service.
class InputSink {
public:
    virtual ~InputSink() = default;
    virtual void touch(int action, int x, int y, int fw, int fh) = 0;
    virtual void scroll(int x, int y, int fw, int fh, int dx, int dy) = 0;
    virtual void nav(int action) = 0;
    virtual void text(const uint8_t* utf8, size_t len) = 0;
    virtual void key(bool down, int keycode) = 0;
    virtual bool input_ready() = 0;
};

// One connected host. Sends go through a bounded queue drained by a writer thread, so a slow,
// stalled or dead host can never block the encoder or audio threads. When the queue is full,
// video is dropped until the next keyframe (which is requested) and audio is dropped.
class Client : public std::enable_shared_from_this<Client> {
public:
    Client(int fd, InputSink* sink, std::function<void()> request_keyframe);
    ~Client();
    void start();
    void enqueue(Msg&& m);
    void close();
    bool alive() const { return alive_.load(); }

private:
    void writer_loop();
    void reader_loop();
    bool send_all(const uint8_t* p, size_t n);

    int fd_;
    InputSink* sink_;
    std::function<void()> request_keyframe_;
    std::mutex mu_;
    std::condition_variable cv_;
    std::deque<Msg> queue_;
    bool wait_key_ = false;
    std::atomic<bool> alive_{true};
};

class Server {
public:
    Server(InputSink* sink, uint32_t version_code, uint32_t sdk, std::string model);
    ~Server();

    // Listens on the abstract unix socket `name` (adb: localabstract:<name>).
    bool start(const char* name);
    void stop();

    void set_keyframe_requester(std::function<void()> f);

    // Thread-safe; cheap no-ops when no host is connected. Latest config is cached for new hosts.
    void send_video_config(uint32_t w, uint32_t h, uint8_t rotation, const uint8_t* cfg, size_t n);
    // Keyframes are prefixed with `cfg` (SPS/PPS) so a late joiner can start there.
    void send_video_frame(const uint8_t* cfg, size_t cfg_n, const uint8_t* data, size_t n, bool key, uint64_t pts_us);
    void set_audio(uint32_t rate, uint8_t channels, uint8_t status);
    void send_audio(const uint8_t* pcm, size_t n, uint64_t pts_us);

private:
    void accept_loop();
    void status_loop();
    std::shared_ptr<Client> current();
    void send_status(Client& c);

    InputSink* sink_;
    uint32_t version_code_, sdk_;
    std::string model_;
    int listen_fd_ = -1;
    std::atomic<bool> running_{false};
    std::thread accept_thread_, status_thread_;

    std::mutex mu_;
    std::shared_ptr<Client> client_;
    std::function<void()> request_keyframe_;
    std::vector<uint8_t> video_config_;  // payload of the last VIDEO_CONFIG message
    uint32_t audio_rate_ = 0;
    uint8_t audio_channels_ = 0;
    std::atomic<uint8_t> audio_status_{AUDIO_UNKNOWN};
};

// H.264 surface encoder built on the NDK AMediaCodec API.
class Encoder {
public:
    Encoder(Server* server, int width, int height, int rotation, int bitrate);
    ~Encoder();

    bool start();                 // after this window() is valid
    ANativeWindow* window() const { return window_; }
    void request_keyframe();
    void stop();

private:
    void drain_loop();

    Server* server_;
    int width_, height_, rotation_, bitrate_;
    AMediaCodec* codec_ = nullptr;
    ANativeWindow* window_ = nullptr;
    std::thread thread_;
    std::atomic<bool> running_{false};
    std::mutex cfg_mu_;
    std::vector<uint8_t> config_;
};

}  // namespace rd
