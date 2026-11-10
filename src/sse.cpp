#include "omh/sse.hpp"

namespace omh {
SseParser::SseParser(std::function<void(SseEvent)> sink, Limits limits)
    : limits_(limits), sink_(std::move(sink)) {}
void SseParser::utf8(unsigned char c) {
    if (continuation_) {
        require((c & 0xc0) == 0x80, "utf8_continuation", Stage::framing);
        point_ = (point_ << 6) | (c & 0x3f);
        if (--continuation_ == 0)
            require(point_ >= minimum_ && point_ <= 0x10ffff && !(point_ >= 0xd800 && point_ <= 0xdfff),
                    "utf8_codepoint", Stage::framing);
    } else if (c < 0x80) { return; }
    else if (c >= 0xc2 && c <= 0xdf) { continuation_ = 1; point_ = c & 0x1f; minimum_ = 0x80; }
    else if (c >= 0xe0 && c <= 0xef) { continuation_ = 2; point_ = c & 0x0f; minimum_ = 0x800; }
    else if (c >= 0xf0 && c <= 0xf4) { continuation_ = 3; point_ = c & 0x07; minimum_ = 0x10000; }
    else fail(ErrorCode::protocol, Stage::framing, "utf8_lead");
}
void SseParser::line() {
    if (line_.empty()) {
        if (has_data_) {
            require(++events_ <= limits_.events, "sse_event_count", Stage::framing, ErrorCode::limit);
            data_.pop_back(); // Remove the final data-field newline, including for empty data.
            SseEvent event{name_.empty() ? "message" : std::move(name_), std::move(data_)};
            name_.clear(); data_.clear(); has_data_ = false; event_bytes_ = 0;
            sink_(std::move(event));
        }
        name_.clear(); event_bytes_ = 0;
    } else if (line_.front() != ':') {
        const auto split = line_.find(':');
        const std::string_view text(line_);
        const auto key = text.substr(0, split);
        auto value = split == std::string::npos ? std::string_view{} : text.substr(split + 1);
        if (value.starts_with(' ')) value.remove_prefix(1);
        if (key == "data") {
            require(value.size() + 1 <= limits_.event && data_.size() <= limits_.event - value.size() - 1,
                    "sse_data_limit", Stage::framing, ErrorCode::limit);
            data_.append(value); data_ += '\n'; has_data_ = true;
        } else if (key == "event") name_ = value;
        // id/retry/extension fields have no meaning for this one-shot transport.
    }
    line_.clear();
}
void SseParser::byte(char c) {
    if (cr_) { cr_ = false; if (c == '\n') return; }
    require(++event_bytes_ <= limits_.event, "sse_event_limit", Stage::framing, ErrorCode::limit);
    if (c == '\r' || c == '\n') { line(); cr_ = c == '\r'; }
    else {
        require(line_.size() < limits_.line, "sse_line_limit", Stage::framing, ErrorCode::limit);
        line_ += c;
    }
}
void SseParser::feed(std::string_view bytes) {
    require(!closed_ && !poisoned_, "sse_closed", Stage::framing);
    try {
        require(bytes.size() <= limits_.body - bytes_, "sse_body_limit", Stage::framing, ErrorCode::limit);
        bytes_ += bytes.size();
        constexpr std::string_view bom = "\xef\xbb\xbf";
        for (const unsigned char c : bytes) {
            utf8(c);
            if (start_) {
                prefix_ += static_cast<char>(c);
                if (bom.starts_with(prefix_)) {
                    if (prefix_.size() == bom.size()) { start_ = false; prefix_.clear(); }
                    continue;
                }
                start_ = false;
                for (const char p : prefix_) byte(p);
                prefix_.clear();
            } else byte(static_cast<char>(c));
        }
    } catch (...) { poisoned_ = true; throw; }
}
void SseParser::finish() {
    require(!closed_ && !poisoned_, "sse_closed", Stage::framing);
    closed_ = true;
    require(continuation_ == 0, "utf8_eof", Stage::framing);
    require(line_.empty() && !has_data_ && name_.empty(), "sse_unterminated_event", Stage::framing);
}
} // namespace omh
