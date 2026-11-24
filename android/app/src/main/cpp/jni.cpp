// JNI surface of the native core (class com.thunderlight.remote.display.NativeCore).
#include <android/native_window_jni.h>
#include <jni.h>

#include "rdcore.h"

namespace {

JavaVM* g_vm = nullptr;

// Detach the thread from the JVM when it exits.
struct Detacher {
    bool attached = false;
    ~Detacher() {
        if (attached && g_vm) g_vm->DetachCurrentThread();
    }
};

JNIEnv* attach() {
    JNIEnv* env = nullptr;
    if (g_vm->GetEnv(reinterpret_cast<void**>(&env), JNI_VERSION_1_6) == JNI_OK) return env;
    static thread_local Detacher d;
    if (g_vm->AttachCurrentThread(&env, nullptr) == JNI_OK) d.attached = true;
    return env;
}

void clear_exception(JNIEnv* env) {
    if (env->ExceptionCheck()) {
        env->ExceptionDescribe();
        env->ExceptionClear();
    }
}

// Forwards host input to the Kotlin bridge (which hands it to the accessibility service).
class JniSink : public rd::InputSink {
public:
    jobject bridge = nullptr;
    jmethodID m_touch = nullptr, m_scroll = nullptr, m_nav = nullptr, m_text = nullptr, m_key = nullptr, m_ready = nullptr;

    void touch(int action, int x, int y, int fw, int fh) override {
        if (JNIEnv* e = attach()) {
            e->CallVoidMethod(bridge, m_touch, action, x, y, fw, fh);
            clear_exception(e);
        }
    }
    void scroll(int x, int y, int fw, int fh, int dx, int dy) override {
        if (JNIEnv* e = attach()) {
            e->CallVoidMethod(bridge, m_scroll, x, y, fw, fh, dx, dy);
            clear_exception(e);
        }
    }
    void nav(int action) override {
        if (JNIEnv* e = attach()) {
            e->CallVoidMethod(bridge, m_nav, action);
            clear_exception(e);
        }
    }
    void text(const uint8_t* utf8, size_t len) override {
        JNIEnv* e = attach();
        if (!e) return;
        jbyteArray arr = e->NewByteArray(static_cast<jsize>(len));
        e->SetByteArrayRegion(arr, 0, static_cast<jsize>(len), reinterpret_cast<const jbyte*>(utf8));
        e->CallVoidMethod(bridge, m_text, arr);
        e->DeleteLocalRef(arr);
        clear_exception(e);
    }
    void key(bool down, int keycode) override {
        if (JNIEnv* e = attach()) {
            e->CallVoidMethod(bridge, m_key, static_cast<jboolean>(down), keycode);
            clear_exception(e);
        }
    }
    bool input_ready() override {
        JNIEnv* e = attach();
        if (!e) return false;
        const bool r = e->CallBooleanMethod(bridge, m_ready) == JNI_TRUE;
        clear_exception(e);
        return r;
    }
};

JniSink g_sink;
std::unique_ptr<rd::Server> g_server;
std::mutex g_mu;  // guards g_cur / g_retired (never held while joining a thread)
std::unique_ptr<rd::Encoder> g_cur, g_retired;

}  // namespace

extern "C" JNIEXPORT jint JNICALL JNI_OnLoad(JavaVM* vm, void*) {
    g_vm = vm;
    return JNI_VERSION_1_6;
}

extern "C" JNIEXPORT jboolean JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeStart(
    JNIEnv* env, jobject, jobject bridge, jint versionCode, jint sdk, jstring model) {
    if (g_server) return JNI_TRUE;
    g_sink.bridge = env->NewGlobalRef(bridge);
    jclass c = env->GetObjectClass(bridge);
    g_sink.m_touch = env->GetMethodID(c, "onTouch", "(IIIII)V");
    g_sink.m_scroll = env->GetMethodID(c, "onScroll", "(IIIIII)V");
    g_sink.m_nav = env->GetMethodID(c, "onNav", "(I)V");
    g_sink.m_text = env->GetMethodID(c, "onText", "([B)V");
    g_sink.m_key = env->GetMethodID(c, "onKey", "(ZI)V");
    g_sink.m_ready = env->GetMethodID(c, "inputReady", "()Z");
    if (!g_sink.m_touch || !g_sink.m_scroll || !g_sink.m_nav || !g_sink.m_text || !g_sink.m_key || !g_sink.m_ready) {
        LOGE("bridge is missing methods");
        env->ExceptionClear();
        return JNI_FALSE;
    }
    const char* m = env->GetStringUTFChars(model, nullptr);
    std::string model_name = m ? m : "Android";
    if (m) env->ReleaseStringUTFChars(model, m);

    g_server = std::make_unique<rd::Server>(&g_sink, static_cast<uint32_t>(versionCode), static_cast<uint32_t>(sdk), model_name);
    g_server->set_keyframe_requester([] {
        std::lock_guard<std::mutex> l(g_mu);
        if (g_cur) g_cur->request_keyframe();
    });
    if (!g_server->start("remote_display")) {
        g_server.reset();
        return JNI_FALSE;
    }
    return JNI_TRUE;
}

// Create a new encoder and make it current; the previous one keeps running until
// nativeReleaseRetiredEncoder() so the VirtualDisplay can be re-pointed without a gap.
extern "C" JNIEXPORT jobject JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeSwitchEncoder(
    JNIEnv* env, jobject, jint width, jint height, jint rotation, jint bitrate) {
    if (!g_server) return nullptr;
    auto enc = std::make_unique<rd::Encoder>(g_server.get(), width, height, rotation, bitrate);
    if (!enc->start()) return nullptr;
    jobject surface = ANativeWindow_toSurface(env, enc->window());

    std::unique_ptr<rd::Encoder> victim;
    {
        std::lock_guard<std::mutex> l(g_mu);
        victim = std::move(g_retired);  // an older, already-replaced encoder
        g_retired = std::move(g_cur);
        g_cur = std::move(enc);
    }
    victim.reset();  // joins its thread outside the lock
    return surface;
}

extern "C" JNIEXPORT void JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeReleaseRetiredEncoder(JNIEnv*, jobject) {
    std::unique_ptr<rd::Encoder> victim;
    {
        std::lock_guard<std::mutex> l(g_mu);
        victim = std::move(g_retired);
    }
    victim.reset();
}

extern "C" JNIEXPORT void JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeSetAudio(JNIEnv*, jobject, jint rate, jint channels, jint status) {
    if (g_server) g_server->set_audio(static_cast<uint32_t>(rate), static_cast<uint8_t>(channels), static_cast<uint8_t>(status));
}

// `buffer` must be a direct ByteBuffer: the PCM is read in place, no JNI array copy.
extern "C" JNIEXPORT void JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeSendAudio(JNIEnv* env, jobject, jobject buffer, jint len, jlong ptsUs) {
    if (!g_server) return;
    auto* p = static_cast<uint8_t*>(env->GetDirectBufferAddress(buffer));
    if (p && len > 0) g_server->send_audio(p, static_cast<size_t>(len), static_cast<uint64_t>(ptsUs));
}

extern "C" JNIEXPORT void JNICALL Java_com_thunderlight_remote_display_NativeCore_nativeStop(JNIEnv* env, jobject) {
    std::unique_ptr<rd::Encoder> a, b;
    {
        std::lock_guard<std::mutex> l(g_mu);
        a = std::move(g_cur);
        b = std::move(g_retired);
    }
    a.reset();
    b.reset();
    g_server.reset();  // stops threads, closes the client
    if (g_sink.bridge) {
        env->DeleteGlobalRef(g_sink.bridge);
        g_sink.bridge = nullptr;
    }
}
