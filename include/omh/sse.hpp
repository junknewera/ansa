#pragma once
#include "omh/json.hpp"
#include <functional>

namespace omh {
struct SseEvent { std::string name, data; };
class SseParser {
    Limits limits_;
    std::function<void(SseEvent)> sink_;
    std::string prefix_, line_, name_, data_;
    std::size_t bytes_ = 0, event_bytes_ = 0, events_ = 0;
    unsigned continuation_ = 0;
    std::uint32_t point_ = 0, minimum_ = 0;
    bool start_ = true, cr_ = false, has_data_ = false, closed_ = false, poisoned_ = false;
    void utf8(unsigned char);
    void byte(char);
    void line();
public:
    explicit SseParser(std::function<void(SseEvent)> sink, Limits limits = {});
    void feed(std::string_view);
    void finish(); // Never dispatches a partial event at EOF.
};
} // namespace omh
