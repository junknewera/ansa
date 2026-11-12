#pragma once
#include <algorithm>
#include <locale.h>
#include <condition_variable>
#include <cstdlib>
#include <cwchar>
#include <mutex>
#include <string>
#include <string_view>
#include <sys/ioctl.h>
#include <termios.h>
#include <thread>
#include <unistd.h>
#include <vector>

namespace omh {
// Only this worker writes frames. Input and runtime control do not wait for stdout.
class Terminal {
    termios saved_{};
    bool active_ = false, color_ = false, dirty_ = false, stop_ = false;
    std::mutex mutex_;
    std::condition_variable changed_;
    std::thread writer_;
    std::string frame_;
    std::size_t rows_ = 0;
    std::function<bool(std::string_view)> write_;
    locale_t locale_ = nullptr, previous_locale_ = nullptr;
    struct Glyph { std::string bytes; int width; };
    static int fallback_width(char32_t cp) {
        if ((cp >= 0x0300 && cp <= 0x036f) || (cp >= 0x1ab0 && cp <= 0x1aff) ||
            (cp >= 0x1dc0 && cp <= 0x1dff) || (cp >= 0x20d0 && cp <= 0x20ff) ||
            (cp >= 0xfe00 && cp <= 0xfe0f) || (cp >= 0xfe20 && cp <= 0xfe2f) ||
            (cp >= 0xe0100 && cp <= 0xe01ef) || cp == 0x200d) return 0;
        if ((cp >= 0x1100 && cp <= 0x115f) || cp == 0x2329 || cp == 0x232a ||
            (cp >= 0x2e80 && cp <= 0xa4cf && cp != 0x303f) ||
            (cp >= 0xac00 && cp <= 0xd7a3) || (cp >= 0xf900 && cp <= 0xfaff) ||
            (cp >= 0xfe10 && cp <= 0xfe19) || (cp >= 0xfe30 && cp <= 0xfe6f) ||
            (cp >= 0xff00 && cp <= 0xff60) || (cp >= 0xffe0 && cp <= 0xffe6) ||
            (cp >= 0x1f300 && cp <= 0x1faff) || (cp >= 0x20000 && cp <= 0x3fffd)) return 2;
        return 1;
    }
    static std::vector<Glyph> glyphs(std::string_view text) {
        std::vector<Glyph> result;
        for (std::size_t at = 0; at < text.size();) {
            const auto ch = static_cast<unsigned char>(text[at]);
            if (ch < 32 || ch == 127) {
                result.push_back({ch == '\n' ? "\n" : ch == '\t' ? " " : "?", ch == '\n' ? -1 : 1});
                ++at; continue;
            }
            const std::size_t n = ch < 0x80 ? 1 : ch >= 0xc2 && ch <= 0xdf ? 2 :
                ch >= 0xe0 && ch <= 0xef ? 3 : ch >= 0xf0 && ch <= 0xf4 ? 4 : 0;
            char32_t cp = n == 1 ? ch : n == 2 ? ch & 0x1f : n == 3 ? ch & 0x0f : ch & 0x07;
            bool valid = n && at + n <= text.size();
            for (std::size_t i = 1; valid && i < n; ++i) {
                const auto next = static_cast<unsigned char>(text[at + i]);
                if ((next & 0xc0) != 0x80) valid = false;
                else cp = (cp << 6) | (next & 0x3f);
            }
            if (!valid || (n == 2 && cp < 0x80) || (n == 3 && cp < 0x800) ||
                (n == 4 && cp < 0x10000) || (cp >= 0xd800 && cp <= 0xdfff) || cp > 0x10ffff) {
                result.push_back({"?", 1}); ++at; continue;
            }
            auto width = ::wcwidth(static_cast<wchar_t>(cp));
            if (cp >= 0x80 && cp <= 0x9f) result.push_back({"?", 1});
            else if (width < 0) result.push_back({std::string(text.substr(at, n)), fallback_width(cp)});
            else result.push_back({std::string(text.substr(at, n)), width});
            at += n;
        }
        return result;
    }
public:
    explicit Terminal(std::function<bool(std::string_view)> write) : write_(std::move(write)) {
        require(::tcgetattr(STDIN_FILENO, &saved_) == 0, "cli_termios", Stage::frontend);
        color_ = !std::getenv("NO_COLOR") && (!std::getenv("TERM") || std::string_view(std::getenv("TERM")) != "dumb");
        resume();
        locale_ = ::newlocale(LC_CTYPE_MASK, "C.UTF-8", nullptr);
        if (!locale_) locale_ = ::newlocale(LC_CTYPE_MASK, "", nullptr);
        if (locale_) {
            previous_locale_ = ::uselocale(locale_);
            if (!previous_locale_) { ::freelocale(locale_); locale_ = nullptr; }
        }
        try { writer_ = std::thread([this] {
            std::unique_lock lock(mutex_);
            std::size_t previous = 0;
            while (true) {
                changed_.wait(lock, [&] { return dirty_ || stop_; });
                if (stop_) break;
                auto frame = frame_; const auto rows = rows_; dirty_ = false;
                lock.unlock();
                if (!previous) {
                    (void)write_(std::string(rows - 1, '\n'));
                    (void)write_("\r\033[" + std::to_string(rows - 1) + "A");
                } else (void)write_("\r\033[" + std::to_string(previous - 2) + "A");
                (void)write_(frame);
                previous = rows;
                lock.lock();
            }
        }); } catch (...) {
            restore();
            if (locale_) { (void)::uselocale(previous_locale_); ::freelocale(locale_); locale_ = nullptr; }
            throw;
        }
    }
    ~Terminal() {
        { std::lock_guard lock(mutex_); stop_ = true; changed_.notify_one(); }
        if (writer_.joinable()) writer_.join();
        restore();
        if (locale_) { (void)::uselocale(previous_locale_); ::freelocale(locale_); }
    }
    void restore() { if (active_) { (void)::tcsetattr(STDIN_FILENO, TCSANOW, &saved_); active_ = false; } }
    void resume() {
        auto raw = saved_; raw.c_lflag &= static_cast<tcflag_t>(~(ICANON | ECHO));
        raw.c_cc[VMIN] = 1; raw.c_cc[VTIME] = 0;
        require(::tcsetattr(STDIN_FILENO, TCSANOW, &raw) == 0, "cli_termios", Stage::frontend); active_ = true;
        { std::lock_guard lock(mutex_); if (!frame_.empty()) { dirty_ = true; changed_.notify_one(); } }
    }
    std::size_t render(const std::vector<std::string>& history, std::string_view input, std::size_t cursor,
                std::string_view mode, bool expanded, std::size_t scroll) {
        winsize size{}; (void)::ioctl(STDOUT_FILENO, TIOCGWINSZ, &size);
        if ((size.ws_row && size.ws_row < 5) || (size.ws_col && size.ws_col < 5)) return 0;
        const std::size_t rows = std::clamp<std::size_t>(size.ws_row ? size.ws_row - 1 : 15, 4, 30);
        const std::size_t cols = (size.ws_col ? size.ws_col : 80) - 1;
        std::vector<std::string> wrapped;
        for (const auto& entry : history) {
            std::string line; std::size_t width = 0;
            for (const auto& glyph : glyphs(entry)) {
                if (glyph.width < 0 || (glyph.width > 0 && width + glyph.width > cols)) {
                    wrapped.push_back(std::move(line)); line.clear(); width = 0;
                    if (glyph.width < 0) continue;
                }
                line += glyph.bytes; width += glyph.width;
            }
            wrapped.push_back(std::move(line));
        }
        const auto end = wrapped.size() - std::min(scroll, wrapped.size() > 0 ? wrapped.size() - 1 : 0);
        const auto available = rows - 3;
        const auto begin = end > available ? end - available : 0;
        std::vector<std::string> lines;
        lines.push_back(color_ ? "\033[38;2;133;220;177mansa\033[0m" : "ansa");
        for (auto i = begin; i < end; ++i) lines.push_back(wrapped[i]);
        while (lines.size() < rows - 2) lines.emplace_back();
        auto before = glyphs(input.substr(0, std::min(cursor, input.size())));
        auto after = glyphs(input.substr(std::min(cursor, input.size())));
        std::size_t before_width = 0;
        for (const auto& glyph : before) before_width += std::max(0, glyph.width);
        std::size_t first_visible = 0;
        while (first_visible < before.size() && before_width > cols - 3)
            before_width -= std::max(0, before[first_visible++].width);
        std::string editor = "> "; std::size_t width = 2;
        for (auto i = first_visible; i < before.size(); ++i) { editor += before[i].bytes; width += std::max(0, before[i].width); }
        for (const auto& glyph : after) { if (width + std::max(0, glyph.width) > cols) break; editor += glyph.bytes; width += std::max(0, glyph.width); }
        lines.push_back(std::move(editor));
        std::string status; std::size_t status_width = 0;
        for (const auto& glyph : glyphs(std::string(mode) + (expanded ? "  history: full" : "  history: recent") + "  Ctrl+F history")) {
            if (glyph.width < 0 || status_width + glyph.width > cols) break;
            status += glyph.bytes; status_width += glyph.width;
        }
        lines.push_back(std::move(status));
        std::string out;
        for (std::size_t i = 0; i < rows; ++i) {
            out += "\033[2K"; out += lines[i];
            if (i + 1 < rows) out += "\r\n";
        }
        out += "\033[1A\r\033[" + std::to_string(2 + before_width) + "C";
        { std::lock_guard lock(mutex_);
          if (frame_ != out || rows_ != rows) {
              frame_ = std::move(out); rows_ = rows; dirty_ = true;
              changed_.notify_one();
          }
        }
        return wrapped.size() > available ? wrapped.size() - available : 0;
    }
};
} // namespace omh
