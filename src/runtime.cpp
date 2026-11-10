#include "runtime_inbox.hpp"
#include <openssl/rand.h>
#include <array>
#include <condition_variable>
#include <mutex>
#include <thread>

namespace omh {
namespace {
using namespace detail;
enum class Phase { idle, generating, executing, completed, cancelled, failed };

const char* phase_name(Phase phase) {
    switch (phase) {
        case Phase::idle: return "idle";
        case Phase::generating: return "generating";
        case Phase::executing: return "executing";
        case Phase::completed: return "completed";
        case Phase::cancelled: return "cancelled";
        case Phase::failed: return "failed";
    }
    std::terminate();
}

RuntimeIncarnation new_incarnation() {
    std::array<unsigned char, 16> bytes{};
    require(RAND_bytes(bytes.data(), static_cast<int>(bytes.size())) == 1,
            "incarnation_generation_failed", Stage::frontend);
    constexpr char hex[] = "0123456789abcdef";
    std::string value;
    value.reserve(bytes.size() * 2);
    for (const auto byte : bytes) {
        value += hex[byte >> 4];
        value += hex[byte & 15];
    }
    return {std::move(value)};
}

template<class T> T unwrap(Result<T> result) {
    if (auto* error = std::get_if<Error>(&result)) throw std::move(*error);
    return std::get<T>(std::move(result));
}

std::string final_text(const CompletedResponse& response) {
    constexpr std::size_t limit = 64 * 1024;
    std::string text;
    for (const auto& item : response.items()) {
        if (item.text("type") != "message") continue;
        for (const auto& part : item.at("content").elements()) {
            const auto part_text = part.text(part.text("type") == "refusal" ? "refusal" : "text");
            require(part_text.size() <= limit - text.size(), "runtime_text_limit", Stage::frontend, ErrorCode::limit);
            text += part_text;
        }
    }
    return text;
}
} // namespace

class Runtime::Impl {
    const SessionId session_;
    const RuntimeIncarnation incarnation_ = new_incarnation();
    const ContextEpoch context_;
    const std::shared_ptr<const ToolSnapshot> tools_;
    const Generate generate_;
    const RuntimeLimits limits_;

    std::mutex mutex_;
    std::condition_variable_any changed_;
    OwnerInbox inbox_;
    std::set<std::string> submit_ids_;
    std::deque<RuntimeEvent> events_;
    std::optional<Job> job_;
    std::optional<std::stop_source> stop_signal_;
    bool signalling_stop_ = false;
    bool owner_done_ = false;

    // Only the control owner changes turn state. post() reads it to route Cancel.
    std::optional<WorkIdentity> work_;
    std::optional<CommandId> submit_;
    Phase phase_ = Phase::idle;
    bool worker_busy_ = false;
    std::uint64_t sequence_ = 0, next_request_ = 0;
    std::size_t completed_tools_ = 0;
    std::vector<ToolActivity> tool_activity_;
    std::string text_, diagnostic_;
    std::stop_source stop_;
    std::optional<OperationId> recovery_operation_;

    std::jthread worker_, stop_worker_, owner_;

    bool active() const { return phase_ == Phase::generating || phase_ == Phase::executing; }
    const TurnId& turn() const {
        static const TurnId idle{"idle"};
        return work_ ? work_->turn : idle;
    }
    std::optional<RequestId> request() const {
        if (work_) return work_->request;
        return std::nullopt;
    }

    void publish(EventPayload payload, const TurnId& event_turn, std::optional<RequestId> event_request) {
        if (events_.size() == limits_.events) {
            events_.clear();
            ++sequence_;
            events_.push_back({1, sequence_, session_, turn(), incarnation_, request(), ResyncRequired{sequence_ + 1}});
        }
        ++sequence_;
        if (auto* snapshot = std::get_if<FrontendSnapshot>(&payload)) snapshot->through_sequence = sequence_;
        events_.push_back({1, sequence_, session_, event_turn, incarnation_, std::move(event_request), std::move(payload)});
        changed_.notify_all();
    }

    void publish(EventPayload payload) { publish(std::move(payload), turn(), request()); }

    void snapshot(std::optional<CommandId> command = {}) {
        publish(FrontendSnapshot{0, phase_name(phase_), request(), worker_busy_, completed_tools_,
                                 text_, diagnostic_, recovery_operation_, submit_, std::move(command), tool_activity_});
    }

    void reject(const RuntimeCommand& command, std::string reason) {
        publish(RuntimeError{command.command, std::move(reason)}, command.turn, std::nullopt);
    }

    void fail_turn(std::string diagnostic) {
        phase_ = Phase::failed;
        diagnostic_ = valid_id(diagnostic) ? std::move(diagnostic) : "worker_failure";
        publish(RuntimeError{submit_, diagnostic_});
        snapshot();
    }

    void schedule(Task task, Phase phase) {
        job_.emplace(Job{*work_, stop_.get_token(), std::move(task)});
        worker_busy_ = true;
        phase_ = phase;
    }

    void submit(RuntimeCommand command) {
        if (recovery_operation_) {
            reject(command, "recovery_required");
            return;
        }
        // The bounded Submit ID set also bounds the number of request IDs in this incarnation.
        work_ = WorkIdentity{incarnation_, std::move(command.turn), {"request_" + std::to_string(++next_request_)}};
        submit_ = std::move(command.command);
        stop_ = std::stop_source{};
        completed_tools_ = 0;
        tool_activity_.clear();
        text_.clear();
        diagnostic_.clear();
        schedule(InitialPrompt{std::get<Submit>(std::move(command.payload)).text,
                               OperationIds(incarnation_.value + "_" + work_->request.value)}, Phase::generating);
        publish(CommandAccepted{*submit_, request()});
        snapshot();
    }

    void record_tools(const std::shared_ptr<const std::vector<ToolResult>>& results) {
        for (std::size_t i = 0; i < tool_activity_.size(); ++i) {
            const auto& result = (*results)[i];
            // Events and snapshots share the immutable owning batch results.
            ToolFinished finished{{result.operation_id}, result.call_id, result.execution, result.effects,
                                  std::shared_ptr<const std::string>(results, &result.output)};
            tool_activity_[i] = finished;
            publish(std::move(finished));
            if (result.effects == Effects::known_result) ++completed_tools_;
            if (result.effects == Effects::unknown) {
                recovery_operation_ = OperationId{result.operation_id};
                diagnostic_ = "tool_effect_unknown";
                publish(RecoveryRequired{*recovery_operation_});
            }
        }
    }

    void complete(Completion done) {
        if (!worker_busy_ || work_ != done.identity) return;
        worker_busy_ = false;
        auto* replay = std::get_if<Replay>(&done.result);
        if (replay) record_tools(replay->results);
        if (phase_ == Phase::cancelled) {
            snapshot();
        } else if (auto* error = std::get_if<Error>(&done.result)) {
            fail_turn(std::move(error->diagnostic));
        } else if (auto* delta = std::get_if<TextDelta>(&done.result)) {
            text_ = delta->text;
            phase_ = Phase::completed;
            if (!text_.empty()) publish(std::move(*delta));
            publish(Done{});
            snapshot();
        } else if (auto* tools = std::get_if<RunTools>(&done.result)) {
            schedule(std::move(*tools), Phase::executing);
            snapshot();
        } else if (recovery_operation_) {
            fail_turn("tool_effect_unknown");
        } else if (std::any_of(replay->results->begin(), replay->results->end(), [](const ToolResult& result) {
                       return result.execution != Execution::succeeded;
                   })) {
            fail_turn("tool_failed");
        } else {
            schedule(std::move(*replay), Phase::generating);
            snapshot();
        }
    }

    void signal_stop() {
        if (signalling_stop_) return;
        stop_signal_ = stop_;
        signalling_stop_ = true;
        changed_.notify_all();
    }

    void run_stop_worker(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        while (changed_.wait(lock, thread_stop, [&] { return stop_signal_ || owner_done_; }) &&
               !thread_stop.stop_requested() && stop_signal_) {
            auto source = std::move(*stop_signal_);
            stop_signal_.reset();
            lock.unlock();
            // request_stop() can execute provider callbacks synchronously.
            source.request_stop();
            lock.lock();
            signalling_stop_ = false;
            changed_.notify_all();
        }
    }

    void run_owner(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        snapshot();
        const auto next = [&] { return inbox_.next(worker_busy_ || signalling_stop_); };
        const auto ready = [&] { return next() != Action::wait; };
        while (changed_.wait(lock, thread_stop, ready) && !thread_stop.stop_requested()) {
            const auto action = next();
            if (action == Action::stop) break;
            switch (action) {
                case Action::tool_started:
                    tool_activity_.emplace_back(inbox_.tool_starts.front());
                    publish(std::move(inbox_.tool_starts.front()));
                    inbox_.tool_starts.pop_front();
                    break;
                case Action::cancel: {
                    auto command = std::move(*inbox_.cancel);
                    inbox_.cancel.reset();
                    phase_ = Phase::cancelled;
                    // A task still in the mailbox has not been dispatched to the worker.
                    if (job_) {
                        job_.reset();
                        worker_busy_ = false;
                    }
                    publish(Cancelled{std::move(command.command)});
                    snapshot();
                    signal_stop();
                    break;
                }
                case Action::complete: {
                    auto done = std::move(*inbox_.completion);
                    inbox_.completion.reset();
                    complete(std::move(done));
                    break;
                }
                case Action::snapshot: {
                    auto command = std::move(*inbox_.snapshot);
                    inbox_.snapshot.reset();
                    if (std::get<GetSnapshot>(command.payload).after_sequence > sequence_)
                        reject(command, "resync_cursor_ahead");
                    else snapshot(std::move(command.command));
                    break;
                }
                case Action::submit: {
                    auto command = std::move(inbox_.submits.front());
                    inbox_.submits.pop_front();
                    submit(std::move(command));
                    break;
                }
                case Action::stop:
                case Action::wait: std::terminate();
            }
        }
        if (worker_busy_) signal_stop();
        owner_done_ = true;
        job_.reset();
        lock.unlock();
        changed_.notify_all();
    }

    WorkerResult generate(const Request& request, const Job& job, OperationIds* operations = nullptr) {
        {
            std::lock_guard lock(mutex_);
            require(can_dispatch(job.identity), "generation_cancelled", Stage::admission, ErrorCode::cancelled);
        }
        auto response = unwrap(generate_(request, job.stop));
        const auto& origin = response.request();
        require(response.returned_model() == request.binding().returned_model &&
                origin.binding() == request.binding() && origin.epoch().id == request.epoch().id &&
                origin.epoch().exact_state == request.epoch().exact_state &&
                origin.tools().revision == request.tools().revision && origin.json() == request.json(),
                "provider_request_mismatch", Stage::admission);
        require(response.calls().size() <= Limits{}.calls && response.items().size() <= Limits{}.items,
                "runtime_response_limit", Stage::admission, ErrorCode::limit);
        if (response.calls().empty()) return TextDelta{final_text(response)};
        require(operations != nullptr, "tool_round_limit", Stage::admission, ErrorCode::limit);
        auto batch = unwrap(PreparedBatch::prepare(response, tools_, *operations));
        return RunTools{std::move(response), std::move(batch)};
    }

    bool can_dispatch(const WorkIdentity& identity) const {
        return !inbox_.aborting && !owner_done_ && !inbox_.cancel && active() && work_ == identity;
    }

    WorkerResult evaluate(Job& job) {
        try {
            if (auto* first = std::get_if<InitialPrompt>(&job.task)) {
                Request request({}, context_, tools_->advertisement(),
                    {Json::object({{"role", Json::string("user")}, {"content", Json::string(first->text)}})});
                return generate(request, job, &first->operations);
            }
            if (auto* tools = std::get_if<RunTools>(&job.task)) {
                auto results = std::make_shared<std::vector<ToolResult>>();
                *results = unwrap(tools->batch.execute(job.stop, [&](std::string_view operation, std::string_view call) {
                    std::lock_guard lock(mutex_);
                    if (!can_dispatch(job.identity)) return false;
                    inbox_.tool_starts.push_back({{std::string(operation)}, std::string(call)});
                    changed_.notify_all();
                    return true;
                }));
                return Replay{std::move(tools->response), std::move(results)};
            }
            const auto& replay = std::get<Replay>(job.task);
            return generate(continuation(replay.response, *replay.results), job);
        } catch (Error& error) {
            return std::move(error);
        } catch (...) {
            return Error(ErrorCode::contract, Stage::admission, "worker_failure");
        }
    }

    void run_worker(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        const auto ready = [&] { return inbox_.aborting || owner_done_ || (job_ && can_dispatch(job_->identity)); };
        while (changed_.wait(lock, thread_stop, ready) &&
               !inbox_.aborting && !owner_done_ && !thread_stop.stop_requested()) {
            auto job = std::move(*job_);
            job_.reset();
            lock.unlock();
            auto result = evaluate(job);
            lock.lock();
            if (inbox_.aborting || owner_done_) break;
            inbox_.completion.emplace(Completion{std::move(job.identity), std::move(result)});
            changed_.notify_all();
        }
    }

public:
    Impl(SessionId session, ContextEpoch context, std::shared_ptr<const ToolSnapshot> tools,
         Generate generate, RuntimeLimits limits)
        : session_(std::move(session)), context_(std::move(context)), tools_(std::move(tools)),
          generate_(std::move(generate)), limits_(limits) {
        require(valid_id(session_.value) && bool(tools_) && bool(generate_) &&
                limits_.commands > 0 && limits_.commands <= 256 && limits_.events >= 2 && limits_.events <= 256 &&
                limits_.submit_ids > 0 && limits_.submit_ids <= 65536,
                "runtime_configuration", Stage::frontend, ErrorCode::contract);
        worker_ = std::jthread([this](std::stop_token stop) { run_worker(stop); });
        stop_worker_ = std::jthread([this](std::stop_token stop) { run_stop_worker(stop); });
        owner_ = std::jthread([this](std::stop_token stop) { run_owner(stop); });
    }

    ~Impl() {
        {
            std::lock_guard lock(mutex_);
            inbox_.aborting = true;
        }
        changed_.notify_all();
        owner_.join();
        worker_.join();
        stop_worker_.join();
    }

    const RuntimeIncarnation& incarnation() const { return incarnation_; }

    Result<PostStatus> post(RuntimeCommand command) {
        return capture([&] {
            validate(command);
            require(command.session == session_, "unknown_session", Stage::frontend, ErrorCode::contract);
            if (command.incarnation != incarnation_) return PostStatus::stale_incarnation;
            std::lock_guard lock(mutex_);
            if (inbox_.input_closed || inbox_.aborting || owner_done_) return PostStatus::closed;
            if (const auto* cancel = std::get_if<Cancel>(&command.payload)) {
                if (!active() || !work_ || work_->request != cancel->target || work_->turn != command.turn)
                    return PostStatus::stale_target;
                if (inbox_.cancel) return PostStatus::duplicate;
                inbox_.cancel = std::move(command);
            } else if (std::holds_alternative<GetSnapshot>(command.payload)) {
                if (inbox_.snapshot) return PostStatus::queue_full;
                inbox_.snapshot = std::move(command);
            } else {
                if (submit_ids_.contains(command.command.value)) return PostStatus::duplicate;
                if (submit_ids_.size() == limits_.submit_ids) return PostStatus::command_id_limit;
                if (inbox_.submits.size() == limits_.commands) return PostStatus::queue_full;
                const auto id = submit_ids_.insert(command.command.value).first;
                try { inbox_.submits.push_back(std::move(command)); }
                catch (...) { submit_ids_.erase(id); throw; }
            }
            changed_.notify_all();
            return PostStatus::accepted;
        });
    }

    std::optional<RuntimeEvent> next_event(std::stop_token stop) {
        std::unique_lock lock(mutex_);
        changed_.wait(lock, stop, [&] { return owner_done_ || !events_.empty(); });
        if (events_.empty() || stop.stop_requested()) return std::nullopt;
        auto event = std::move(events_.front());
        events_.pop_front();
        return event;
    }

    void close_input() {
        std::lock_guard lock(mutex_);
        inbox_.input_closed = true;
        changed_.notify_all();
    }
};

Runtime::Runtime(SessionId session, ContextEpoch context, std::shared_ptr<const ToolSnapshot> tools,
                 Generate generate, RuntimeLimits limits)
    : impl_(std::make_unique<Impl>(std::move(session), std::move(context), std::move(tools), std::move(generate), limits)) {}
Runtime::~Runtime() = default;
const RuntimeIncarnation& Runtime::incarnation() const { return impl_->incarnation(); }
Result<PostStatus> Runtime::post(RuntimeCommand command) { return impl_->post(std::move(command)); }
std::optional<RuntimeEvent> Runtime::next_event(std::stop_token stop) { return impl_->next_event(stop); }
void Runtime::close_input() { impl_->close_input(); }
} // namespace omh
