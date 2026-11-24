#include "rdcore.h"

namespace rd {

namespace {
constexpr int32_t kColorFormatSurface = 0x7F000789;
constexpr int32_t kAvcProfileBaseline = 1;
constexpr int32_t kBitrateModeVbr = 1;
// AMediaCodecBufferInfo.flags
constexpr uint32_t kFlagCodecConfig = 2;
constexpr uint32_t kFlagKeyFrame = 1;
}  // namespace

Encoder::Encoder(Server* server, int width, int height, int rotation, int bitrate)
    : server_(server), width_(width), height_(height), rotation_(rotation), bitrate_(bitrate) {}

Encoder::~Encoder() { stop(); }

bool Encoder::start() {
    codec_ = AMediaCodec_createEncoderByType("video/avc");
    if (!codec_) {
        LOGE("no H.264 encoder");
        return false;
    }
    AMediaFormat* f = AMediaFormat_new();
    // Plain string keys (identical to the Java MediaFormat constants): the AMEDIAFORMAT_KEY_*
    // symbols for several of these only exist on newer API levels than our minSdk.
    AMediaFormat_setString(f, "mime", "video/avc");
    AMediaFormat_setInt32(f, "width", width_);
    AMediaFormat_setInt32(f, "height", height_);
    AMediaFormat_setInt32(f, "color-format", kColorFormatSurface);
    AMediaFormat_setInt32(f, "bitrate", bitrate_);
    AMediaFormat_setInt32(f, "frame-rate", 60);
    AMediaFormat_setInt32(f, "i-frame-interval", 2);
    AMediaFormat_setInt32(f, "profile", kAvcProfileBaseline);
    AMediaFormat_setInt32(f, "bitrate-mode", kBitrateModeVbr);
    // Static screens produce no new frames; re-emit so the host keeps getting pictures.
    AMediaFormat_setInt64(f, "repeat-previous-frame-after", 100000);
    AMediaFormat_setInt32(f, "priority", 0);  // realtime
    AMediaFormat_setInt32(f, "latency", 1);
    AMediaFormat_setInt32(f, "max-b-frames", 0);

    media_status_t st = AMediaCodec_configure(codec_, f, nullptr, nullptr, AMEDIACODEC_CONFIGURE_FLAG_ENCODE);
    AMediaFormat_delete(f);
    if (st != AMEDIA_OK) {
        LOGE("configure failed: %d", st);
        return false;
    }
    st = AMediaCodec_createInputSurface(codec_, &window_);
    if (st != AMEDIA_OK || !window_) {
        LOGE("createInputSurface failed: %d", st);
        return false;
    }
    st = AMediaCodec_start(codec_);
    if (st != AMEDIA_OK) {
        LOGE("start failed: %d", st);
        return false;
    }
    running_ = true;
    thread_ = std::thread([this] { drain_loop(); });
    LOGI("encoder started %dx%d rot=%d %d bps", width_, height_, rotation_, bitrate_);
    return true;
}

void Encoder::request_keyframe() {
    if (!codec_ || !running_) return;
    AMediaFormat* p = AMediaFormat_new();
    AMediaFormat_setInt32(p, "request-sync", 0);
    AMediaCodec_setParameters(codec_, p);
    AMediaFormat_delete(p);
}

void Encoder::drain_loop() {
    AMediaCodecBufferInfo info;
    std::vector<uint8_t> config;
    while (running_) {
        const ssize_t idx = AMediaCodec_dequeueOutputBuffer(codec_, &info, 10000);
        if (idx >= 0) {
            size_t cap = 0;
            uint8_t* buf = AMediaCodec_getOutputBuffer(codec_, static_cast<size_t>(idx), &cap);
            if (buf && info.size > 0) {
                const uint8_t* p = buf + info.offset;
                const size_t n = static_cast<size_t>(info.size);
                if (info.flags & kFlagCodecConfig) {
                    config.assign(p, p + n);
                    server_->send_video_config(static_cast<uint32_t>(width_), static_cast<uint32_t>(height_), static_cast<uint8_t>(rotation_), p, n);
                } else {
                    const bool key = (info.flags & kFlagKeyFrame) != 0;
                    // Keyframes carry SPS/PPS so a late-joining decoder/recorder can start there.
                    server_->send_video_frame(key ? config.data() : nullptr, key ? config.size() : 0, p, n, key,
                                              static_cast<uint64_t>(info.presentationTimeUs));
                }
            }
            AMediaCodec_releaseOutputBuffer(codec_, static_cast<size_t>(idx), false);
        } else if (idx == AMEDIACODEC_INFO_TRY_AGAIN_LATER || idx == AMEDIACODEC_INFO_OUTPUT_FORMAT_CHANGED ||
                   idx == AMEDIACODEC_INFO_OUTPUT_BUFFERS_CHANGED) {
            continue;
        } else {
            LOGW("dequeueOutputBuffer: %zd", idx);
            break;
        }
    }
}

void Encoder::stop() {
    running_ = false;
    if (thread_.joinable()) {
        // The last reference can be dropped on the drain thread itself; never join ourselves.
        if (thread_.get_id() == std::this_thread::get_id()) {
            thread_.detach();
        } else {
            thread_.join();
        }
    }
    if (codec_) {
        AMediaCodec_stop(codec_);
        AMediaCodec_delete(codec_);
        codec_ = nullptr;
    }
    if (window_) {
        ANativeWindow_release(window_);
        window_ = nullptr;
    }
}

}  // namespace rd
