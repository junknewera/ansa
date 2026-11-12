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
struct TerminalEntry { std::string role, text; };
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
                if (previous > rows) {
                    std::string clear = "\0337\033[1B";
                    for (std::size_t i = rows; i < previous; ++i) clear += "\r\n\033[2K";
                    clear += "\0338";
                    (void)write_(clear);
                }
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
    std::size_t render(const std::vector<TerminalEntry>& history, std::string_view input, std::size_t cursor,
                std::string_view mode, bool expanded, std::size_t scroll, std::string_view context = {}) {
        winsize size{}; (void)::ioctl(STDOUT_FILENO, TIOCGWINSZ, &size);
        if ((size.ws_row && size.ws_row < 5) || (size.ws_col && size.ws_col < 5)) return 0;
        const std::size_t max_rows = std::clamp<std::size_t>(size.ws_row ? size.ws_row - 1 : 15, 4, 30);
        const std::size_t cols = (size.ws_col ? size.ws_col : 80) - 1;
        const auto mint = color_ ? "\033[38;2;133;220;177m" : "";
        const auto reset = color_ ? "\033[0m" : "";
        std::vector<std::string> wrapped;
        for (const auto& entry : history) {
            if (!wrapped.empty()) wrapped.emplace_back();
            std::string safe;
            for (const auto& glyph : glyphs(entry.text)) safe += glyph.bytes;
            const bool assistant = entry.role == "assistant", user = entry.role == "user";
            bool fence = false, first = true;
            std::size_t start = 0;
            while (start <= safe.size()) {
                const auto newline = safe.find('\n', start);
                auto text = safe.substr(start, newline == std::string::npos ? std::string::npos : newline - start);
                start = newline == std::string::npos ? safe.size() + 1 : newline + 1;
                if (assistant && text.starts_with("```")) { fence = !fence; continue; }
                bool heading = false;
                if (assistant && !fence && text.starts_with("#")) {
                    const auto end = text.find_first_not_of('#');
                    if (end != std::string::npos && text[end] == ' ') { text.erase(0, end + 1); heading = true; }
                }
                struct Styled { Glyph glyph; bool bold, code; };
                std::vector<Styled> chars;
                if (user && first) {
                    chars.push_back({{">", 1}, true, true}); chars.push_back({{" ", 1}, true, true});
                }
                bool bold = false, code = user;
                for (std::size_t at = 0; at < text.size();) {
                    if (assistant && !fence && text.compare(at, 2, "**") == 0 &&
                        (bold || text.find("**", at + 2) != std::string::npos)) {
                        bold = !bold; at += 2; continue;
                    }
                    if (assistant && !fence && text[at] == '`' && (code || text.find('`', at + 1) != std::string::npos)) {
                        code = !code; ++at; continue;
                    }
                    auto next = at + 1;
                    while (next < text.size() && (static_cast<unsigned char>(text[next]) & 0xc0) == 0x80) ++next;
                    for (auto& glyph : glyphs(std::string_view(text).substr(at, next - at))) chars.push_back({std::move(glyph), bold || heading || user, code});
                    at = next;
                }
                std::size_t pos = 0;
                do {
                    auto end = pos, last_space = chars.size(); std::size_t width = 0;
                    while (end < chars.size() && width + std::max(0, chars[end].glyph.width) <= cols) {
                        if (chars[end].glyph.bytes == " " && end > pos) last_space = end;
                        width += std::max(0, chars[end].glyph.width); ++end;
                    }
                    if (end < chars.size() && last_space < end) end = last_space;
                    if (end == pos && end < chars.size()) ++end;
                    std::string line; bool was_bold = false, was_code = false;
                    for (auto i = pos; i < end; ++i) {
                        if (color_ && (chars[i].bold != was_bold || chars[i].code != was_code)) {
                            line += reset;
                            if (chars[i].bold) line += "\033[1m";
                            if (chars[i].code) line += mint;
                            was_bold = chars[i].bold; was_code = chars[i].code;
                        }
                        line += chars[i].glyph.bytes;
                    }
                    if (color_) line += reset;
                    wrapped.push_back(std::move(line));
                    pos = end;
                    if (pos < chars.size() && chars[pos].glyph.bytes == " ") ++pos;
                } while (pos < chars.size());
                first = false;
            }
        }
        const auto available = max_rows - 3;
        const auto end = wrapped.size() - std::min(scroll, wrapped.empty() ? 0UL : wrapped.size() - 1);
        const auto begin = end > available ? end - available : 0;
        std::vector<std::string> lines;
        std::string header = std::string(mint) + (color_ ? "\033[1m" : "") + "ansa" + reset;
        std::size_t header_width = 4;
        for (const auto& glyph : glyphs(context)) {
            if (glyph.width < 0 || header_width + glyph.width + 2 > cols) break;
            if (header_width == 4) { header += "  "; header_width += 2; }
            header += glyph.bytes; header_width += glyph.width;
        }
        lines.push_back(std::move(header));
        for (auto i = begin; i < end; ++i) lines.push_back(wrapped[i]);
        const std::size_t rows = lines.size() + 2;
        auto before = glyphs(input.substr(0, std::min(cursor, input.size())));
        auto after = glyphs(input.substr(std::min(cursor, input.size())));
        std::size_t before_width = 0;
        for (const auto& glyph : before) before_width += std::max(0, glyph.width);
        std::size_t first_visible = 0;
        while (first_visible < before.size() && before_width > cols - 3)
            before_width -= std::max(0, before[first_visible++].width);
        std::string editor = std::string(color_ ? "\033[48;2;42;46;48m" : "") + mint + "> " + (color_ ? "\033[39m" : ""); std::size_t width = 2;
        for (auto i = first_visible; i < before.size(); ++i) { editor += before[i].bytes; width += std::max(0, before[i].width); }
        for (const auto& glyph : after) { if (width + std::max(0, glyph.width) > cols) break; editor += glyph.bytes; width += std::max(0, glyph.width); }
        if (color_) { editor.append(cols - width, ' '); editor += reset; }
        lines.push_back(std::move(editor));
        std::string status; std::size_t status_width = 0;
        for (const auto& glyph : glyphs(std::string(mode) + "  Tab: режим  Ctrl+F: история" + (expanded ? "  PgUp/PgDn" : ""))) {
            if (glyph.width < 0 || status_width + glyph.width > cols) break;
            status += glyph.bytes; status_width += glyph.width;
        }
        lines.push_back(std::string(mint) + status + reset);
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
