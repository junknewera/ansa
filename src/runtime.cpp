#include "runtime_inbox.hpp"
#include "omh/session.hpp"
#include <openssl/rand.h>
#include <array>
#include <condition_variable>
#include <mutex>
#include <thread>

namespace omh {
namespace {
using namespace detail;
template<class T> T unwrap(Result<T> result) {
    if (auto* error = std::get_if<Error>(&result)) throw std::move(*error);
    return std::get<T>(std::move(result));
}
RuntimeIncarnation new_incarnation() {
    std::array<unsigned char, 16> bytes{};
    require(RAND_bytes(bytes.data(), static_cast<int>(bytes.size())) == 1,
            "incarnation_generation_failed", Stage::frontend);
    constexpr char hex[] = "0123456789abcdef";
    std::string value;
    for (const auto byte : bytes) { value += hex[byte >> 4]; value += hex[byte & 15]; }
    return {std::move(value)};
}
Json number(std::size_t value) { return Json::parse(std::to_string(value)); }
Json transition(const TurnId& turn, const char* kind, std::vector<std::pair<std::string, Json>> fields = {}) {
    fields.emplace_back("kind", Json::string(kind));
    fields.emplace_back("turn", Json::string(turn.value));
    return Json::object(fields);
}
enum class Commit { accepted, response, intent, outcome, continued, compacted, done, pause, cancel, resume };
struct StoreJob {
    std::shared_ptr<const SessionState> base;
    std::function<Json()> build;
    Commit commit;
};
struct StoreDone { Commit commit; std::shared_ptr<SessionState> next; std::optional<Error> error; };
struct Checkpoint {
    WorkIdentity identity;
    std::variant<ToolInvocation, ToolResult> value;
};
} // namespace

class Runtime::Impl {
    const RuntimeIncarnation incarnation_ = new_incarnation();
    const std::shared_ptr<const ToolSnapshot> tools_;
    const Generate generate_;
    const Compact compact_;
    const RuntimeLimits limits_;
    const std::size_t profile_bytes_;
    std::shared_ptr<SessionState> state_;
    std::unique_ptr<Journal> journal_;

    std::mutex mutex_;
    std::condition_variable_any changed_;
    OwnerInbox inbox_;
    std::map<std::string, SubmitReceipt> receipts_;
    std::deque<RuntimeEvent> events_;
    std::optional<Job> job_;
    std::optional<StoreJob> store_job_;
    std::optional<StoreDone> store_done_;
    std::optional<Checkpoint> checkpoint_;
    std::optional<bool> checkpoint_reply_;
    std::optional<std::stop_source> stop_signal_;
    std::optional<RuntimeCommand> cancelling_;
    std::optional<CommandId> resuming_;
    std::optional<SubmitReceipt> pending_receipt_;
    bool owner_done_ = false, worker_busy_ = false, storing_ = false, signalling_stop_ = false;
    bool storage_failed_ = false, recovered_pause_ = false, shutting_down_ = false;
    std::string phase_ = "idle", diagnostic_;
    std::optional<OperationId> recovery_;
    std::vector<ToolActivity> activity_;
    std::uint64_t sequence_ = 0;
    std::stop_source stop_;
    std::jthread worker_, storage_worker_, stop_worker_, owner_;

    const TurnId& turn() const {
        static const TurnId idle{"idle"};
        if (pending_receipt_) return pending_receipt_->turn;
        return state_->turn ? state_->turn->receipt.turn : idle;
    }
    std::optional<RequestId> request() const {
        if (pending_receipt_) return pending_receipt_->request;
        if (state_->turn) return state_->turn->receipt.request;
        return {};
    }
    WorkIdentity identity() const { return {incarnation_, turn(), *request()}; }
    bool can_cancel() const {
        return (pending_receipt_ || state_->turn) && phase_ != "completed" && phase_ != "cancelled" && phase_ != "finishing";
    }
    bool can_dispatch(const WorkIdentity& id) const {
        return !inbox_.aborting && !owner_done_ && !inbox_.cancel && !cancelling_ && !storage_failed_ && !recovery_ &&
            !recovered_pause_ && state_->turn && !state_->turn->paused && phase_ != "cancelled" &&
            identity() == id;
    }

    void publish(EventPayload payload, const TurnId& event_turn, std::optional<RequestId> event_request) {
        if (events_.size() == limits_.events) {
            events_.clear();
            ++sequence_;
            events_.push_back({1, sequence_, state_->session, turn(), incarnation_, request(), ResyncRequired{sequence_ + 1}});
        }
        ++sequence_;
        if (auto* view = std::get_if<FrontendSnapshot>(&payload)) view->through_sequence = sequence_;
        events_.push_back({1, sequence_, state_->session, event_turn, incarnation_, std::move(event_request), std::move(payload)});
        changed_.notify_all();
    }
    void publish(EventPayload payload) { publish(std::move(payload), turn(), request()); }

    void snapshot(std::optional<CommandId> command = {}) {
        std::size_t completed = 0;
        for (const auto& tool : activity_)
            if (const auto* result = std::get_if<ToolFinished>(&tool); result && result->effects == Effects::known_result)
                ++completed;
        std::string text;
        if (!pending_receipt_ && state_->turn && state_->turn->response && state_->turn->response->calls().empty())
            text = response_text(*state_->turn->response);
        publish(FrontendSnapshot{0, phase_, request(), worker_busy_ || storing_, completed, std::move(text),
            diagnostic_, recovery_, pending_receipt_ ? std::optional<CommandId>{pending_receipt_->command} :
                state_->turn ? std::optional<CommandId>{state_->turn->receipt.command} : std::nullopt,
            std::move(command), activity_, state_->history.size(), state_->compactions, storage_failed_,
            !storing_ && !storage_failed_, state_->turn && phase_ == "paused" && !recovery_ && !storage_failed_});
    }

    void reject(const RuntimeCommand& command, std::string diagnostic) {
        publish(RuntimeError{command.command, std::move(diagnostic)}, command.turn, {});
    }
    Json event(const char* kind, std::vector<std::pair<std::string, Json>> fields = {}) const {
        return transition(turn(), kind, std::move(fields));
    }

    void persist(std::function<Json()> build, Commit commit) {
        require(!storing_, "concurrent_journal_transition", Stage::frontend);
        store_job_.emplace(StoreJob{state_, std::move(build), commit});
        storing_ = true;
        changed_.notify_all();
    }
    void persist(Json value, Commit commit) {
        persist([value = std::move(value)] { return value; }, commit);
    }
    void schedule(Task task, const char* phase) {
        job_.emplace(Job{identity(), stop_.get_token(), std::move(task)});
        worker_busy_ = true;
        phase_ = phase;
        snapshot();
    }

    void signal_stop() {
        if (signalling_stop_) return;
        stop_signal_ = stop_;
        signalling_stop_ = true;
        changed_.notify_all();
    }
    void reply_checkpoint(bool proceed) {
        checkpoint_reply_ = proceed;
        changed_.notify_all();
    }

    void show_outcome(const ToolResult& result) {
        if (!result.dispatched) return;
        auto output = std::make_shared<const std::string>(result.output);
        ToolFinished finished{{result.operation_id}, result.call_id, result.execution, result.effects, std::move(output)};
        const auto found = std::find_if(activity_.begin(), activity_.end(), [&](const ToolActivity& activity) {
            return std::visit([&](const auto& tool) { return tool.operation.value == result.operation_id; }, activity);
        });
        if (found != activity_.end()) *found = finished;
        else activity_.emplace_back(finished);
        publish(std::move(finished));
        if (result.effects == Effects::unknown) {
            recovery_ = OperationId{result.operation_id};
            publish(RecoveryRequired{*recovery_});
        }
    }

    void storage_failure(const Error& error) {
        storage_failed_ = true;
        phase_ = "paused";
        diagnostic_ = valid_id(error.diagnostic) ? error.diagnostic : "journal_failure";
        if (job_) { job_.reset(); worker_busy_ = false; }
        if (!pending_receipt_ && state_->turn) {
            for (const auto& result : state_->turn->results) {
                if (!result.dispatched) continue;
                const auto shown = std::find_if(activity_.begin(), activity_.end(), [&](const ToolActivity& item) {
                    const auto* finished = std::get_if<ToolFinished>(&item);
                    return finished && finished->operation.value == result.operation_id;
                });
                if (shown == activity_.end()) show_outcome(result);
            }
        }
        if (worker_busy_ && !checkpoint_) signal_stop();
        reply_checkpoint(false);
        if (cancelling_) {
            phase_ = "cancelled";
            publish(Cancelled{cancelling_->command});
            cancelling_.reset();
        }
        publish(RuntimeError{pending_receipt_ ? std::optional<CommandId>{pending_receipt_->command} :
            state_->turn ? std::optional<CommandId>{state_->turn->receipt.command} : std::nullopt, diagnostic_});
        snapshot();
    }

    void committed(Commit commit) {
        switch (commit) {
            case Commit::accepted:
                publish(CommandAccepted{state_->turn->receipt.command, request()});
                break;
            case Commit::intent:
                reply_checkpoint(can_dispatch(identity()));
                break;
            case Commit::outcome:
                show_outcome(state_->turn->results.back());
                reply_checkpoint(can_dispatch(identity()) && !recovery_);
                break;
            case Commit::done:
                phase_ = "completed";
                if (const auto text = response_text(*state_->turn->response); !text.empty()) publish(TextDelta{text});
                publish(Done{});
                snapshot();
                break;
            case Commit::pause:
                phase_ = "paused";
                diagnostic_ = state_->turn->diagnostic;
                publish(RuntimeError{state_->turn->receipt.command, diagnostic_});
                snapshot();
                break;
            case Commit::cancel:
                phase_ = "cancelled";
                if (cancelling_) {
                    publish(Cancelled{cancelling_->command});
                    cancelling_.reset();
                }
                snapshot();
                break;
            case Commit::resume:
                recovered_pause_ = false;
                phase_ = state_->turn->stage;
                diagnostic_.clear();
                stop_ = std::stop_source{};
                publish(CommandAccepted{*resuming_, request()});
                resuming_.reset();
                break;
            case Commit::response:
            case Commit::continued:
            case Commit::compacted: break;
        }
    }

    void advance() {
        if (storing_ || storage_failed_) return;
        if (cancelling_ && state_->turn && state_->turn->stage != "cancelled") {
            persist(event("cancel"), Commit::cancel);
            return;
        }
        if (worker_busy_ || recovered_pause_ || !state_->turn || state_->turn->paused || recovery_ ||
            state_->turn->stage == "cancelled" || state_->turn->stage == "completed" || shutting_down_) return;
        const auto& current = *state_->turn;
        if (current.stage == "generating") {
            if (current.tools_response) {
                auto results = std::make_shared<const std::vector<ToolResult>>(current.results);
                schedule(Replay{*current.tools_response, std::move(results)}, "generating");
            } else schedule(GenerateRequest{current.next}, "generating");
        } else if (current.stage == "compacting") {
            schedule(CompactRequest{Request(state_->configuration.binding(), state_->epoch,
                tools_->advertisement(), state_->history)}, "compacting");
        } else if (current.stage == "tools") {
            schedule(PrepareTools{*current.tools_response, current.operation_run, current.results}, "executing");
        } else if (current.stage == "final") {
            phase_ = "finishing";
            persist(event("done"), Commit::done);
        }
    }

    void submit(RuntimeCommand command) {
        auto& receipt = receipts_.at(command.command.value);
        if (recovery_ || storage_failed_) { reject(command, recovery_ ? "recovery_required" : "journal_read_only"); return; }
        auto input = state_->history;
        input.push_back(Json::object({{"role", Json::string("user")},
            {"content", Json::string(std::get<Submit>(command.payload).text)}}));
        Request next(state_->configuration.binding(), state_->epoch, tools_->advertisement(), std::move(input));
        const bool compact = !state_->history.empty() &&
            (next.input().size() >= compact_item_threshold ||
             state_->history_bytes + receipt.text->size() * 6 + profile_bytes_ >= limits_.compact_bytes);
        receipt.request = {"request_" + std::to_string(state_->next_request + 1)};
        const auto run = incarnation_.value + "_" + receipt.request.value;
        const auto request_number = state_->next_request + 1;
        pending_receipt_ = receipt;
        activity_.clear();
        diagnostic_.clear();
        recovered_pause_ = false;
        stop_ = std::stop_source{};
        phase_ = "persisting";
        persist([receipt, next = std::move(next), run, request_number, compact] {
            return Json::object({{"kind", Json::string("accepted")}, {"command", Json::string(receipt.command.value)},
                {"turn", Json::string(receipt.turn.value)}, {"request", Json::string(receipt.request.value)},
                {"request_number", number(request_number)}, {"text", Json::string(*receipt.text)},
                {"operation_run", Json::string(run)}, {"next", save_request(next)},
                {"compact", Json::parse(compact ? "true" : "false")}});
        }, Commit::accepted);
    }

    void cancel(RuntimeCommand command) {
        if (!can_cancel()) return;
        cancelling_ = std::move(command);
        phase_ = "cancelled";
        if (job_) { job_.reset(); worker_busy_ = false; }
        signal_stop();
        if (storage_failed_) {
            publish(Cancelled{cancelling_->command});
            cancelling_.reset();
            snapshot();
        }
    }

    void checkpoint() {
        auto value = std::move(*checkpoint_);
        checkpoint_.reset();
        if (value.identity != identity() || storage_failed_) { reply_checkpoint(false); return; }
        if (const auto* invocation = std::get_if<ToolInvocation>(&value.value)) {
            if (!can_dispatch(value.identity)) { reply_checkpoint(false); return; }
            persist([turn_id = turn(), invocation = *invocation] {
                return transition(turn_id, "intent", {{"invocation", save_invocation(invocation)}});
            }, Commit::intent);
        } else {
            persist([turn_id = turn(), result = std::get<ToolResult>(std::move(value.value))] {
                return transition(turn_id, "outcome", {{"result", save_result(result)}});
            }, Commit::outcome);
        }
    }

    void complete(Completion done) {
        if (!worker_busy_ || !state_->turn || done.identity != identity()) return;
        worker_busy_ = false;
        if (auto* replay = std::get_if<Replay>(&done.result)) {
            if (storage_failed_) {
                for (const auto& result : *replay->results) {
                    const auto started = std::find_if(activity_.begin(), activity_.end(), [&](const ToolActivity& item) {
                        const auto* begin = std::get_if<ToolStarted>(&item);
                        return begin && begin->operation.value == result.operation_id;
                    });
                    if (started != activity_.end()) show_outcome(result);
                }
            }
            // If Cancel won after a durable intent but before dispatch, close that
            // intent with its known not_started result before releasing the turn.
            if (state_->turn->intent && !storage_failed_) {
                const auto index = state_->turn->results.size();
                persist(event("outcome", {{"result", save_result(replay->results->at(index))}}), Commit::outcome);
                inbox_.completion.emplace(Completion{done.identity, std::move(*replay)});
                worker_busy_ = true;
                return;
            }
            if (phase_ == "cancelled" || shutting_down_ || storage_failed_) { snapshot(); return; }
            if (recovery_) { phase_ = "paused"; snapshot(); return; }
            const auto failed = std::any_of(replay->results->begin(), replay->results->end(),
                [](const ToolResult& result) { return result.execution != Execution::succeeded; });
            persist(failed ? event("pause", {{"diagnostic", Json::string("tool_failed")}}) : event("continued"),
                    failed ? Commit::pause : Commit::continued);
            return;
        }
        if (phase_ == "cancelled" || shutting_down_ || storage_failed_) { snapshot(); return; }
        if (auto* error = std::get_if<Error>(&done.result)) {
            const auto diagnostic = valid_id(error->diagnostic) ? error->diagnostic : "worker_failure";
            persist(event("pause", {{"diagnostic", Json::string(diagnostic)}}), Commit::pause);
        } else if (auto* response = std::get_if<CompletedResponse>(&done.result)) {
            persist([turn_id = turn(), response = std::move(*response)] {
                return transition(turn_id, "response", {{"response", save_response(response)}});
            }, Commit::response);
        } else if (auto* prepared = std::get_if<RunTools>(&done.result)) {
            schedule(std::move(*prepared), "executing");
        } else {
            persist([turn_id = turn(), compacted = std::get<CompactedContext>(std::move(done.result))] {
                return transition(turn_id, "compacted", {{"compaction",
                    Json::object({{"request", save_request(compacted.request())}, {"native", compacted.native()}})}});
            }, Commit::compacted);
        }
    }

    bool available() const {
        return !worker_busy_ && !storing_ && !signalling_stop_ && !recovered_pause_ &&
            (!state_->turn || state_->turn->stage == "completed" || state_->turn->stage == "cancelled");
    }

    void run_owner(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        snapshot();
        const auto ready = [&] {
            return (inbox_.aborting && !shutting_down_) || !inbox_.tool_starts.empty() || inbox_.cancel ||
                store_done_ || (!storing_ && checkpoint_) || (!storing_ && inbox_.completion) ||
                !inbox_.lookups.empty() || inbox_.snapshot || (!storing_ && !worker_busy_ && inbox_.resume) ||
                (available() && !inbox_.submits.empty()) ||
                ((inbox_.input_closed || shutting_down_) && !worker_busy_ && !storing_ && !signalling_stop_);
        };
        while (changed_.wait(lock, thread_stop, ready) && !thread_stop.stop_requested()) {
            try {
                if (inbox_.aborting && !shutting_down_) {
                    shutting_down_ = true;
                    inbox_.submits.clear();
                    inbox_.resume.reset();
                    if (can_cancel()) cancel({1, state_->session, turn(), incarnation_, {"shutdown"}, Cancel{*request()}});
                    else if (worker_busy_) signal_stop();
                } else if (!inbox_.tool_starts.empty()) {
                    auto started = std::move(inbox_.tool_starts.front());
                    inbox_.tool_starts.pop_front();
                    activity_.emplace_back(started);
                    publish(std::move(started));
                } else if (inbox_.cancel) {
                    auto command = std::move(*inbox_.cancel);
                    inbox_.cancel.reset();
                    cancel(std::move(command));
                } else if (store_done_) {
                    auto done = std::move(*store_done_);
                    store_done_.reset();
                    storing_ = false;
                    if (done.next) {
                        state_ = std::move(done.next);
                        if (state_->turn) receipts_[state_->turn->receipt.command.value] = state_->turn->receipt;
                        pending_receipt_.reset();
                    }
                    if (done.error) storage_failure(*done.error);
                    else committed(done.commit);
                } else if (!storing_ && checkpoint_) {
                    checkpoint();
                } else if (!storing_ && inbox_.completion) {
                    auto done = std::move(*inbox_.completion);
                    inbox_.completion.reset();
                    complete(std::move(done));
                } else if (!inbox_.lookups.empty()) {
                    const auto id = std::move(inbox_.lookups.front());
                    inbox_.lookups.pop_front();
                    const auto& receipt = receipts_.at(id.value);
                    const auto status = request() == receipt.request ? phase_ : receipt.state;
                    publish(CommandReplayed{id, status}, receipt.turn,
                            receipt.request.value.empty() ? std::nullopt : std::optional<RequestId>{receipt.request});
                } else if (inbox_.snapshot) {
                    auto command = std::move(*inbox_.snapshot);
                    inbox_.snapshot.reset();
                    if (std::get<GetSnapshot>(command.payload).after_sequence > sequence_) reject(command, "resync_cursor_ahead");
                    else snapshot(command.command);
                } else if (!storing_ && !worker_busy_ && inbox_.resume) {
                    auto command = std::move(*inbox_.resume);
                    inbox_.resume.reset();
                    if (storage_failed_ || recovery_) reject(command, storage_failed_ ? "journal_read_only" : "recovery_required");
                    else if (phase_ != "paused" || !state_->turn || request() != std::get<Resume>(command.payload).target)
                        reject(command, "resume_not_available");
                    else if (state_->turn->resumes.contains(command.command.value)) {
                        publish(CommandReplayed{command.command, phase_});
                    } else if (state_->turn->resumes.size() >= 4096) {
                        reject(command, "resume_id_limit");
                    } else {
                        resuming_ = command.command;
                        persist(event("resume", {{"command", Json::string(command.command.value)}}), Commit::resume);
                    }
                } else if (available() && !inbox_.submits.empty()) {
                    auto command = std::move(inbox_.submits.front());
                    inbox_.submits.pop_front();
                    submit(std::move(command));
                } else if ((inbox_.input_closed || shutting_down_) && !worker_busy_ && !storing_ && !signalling_stop_) {
                    break;
                }
                advance();
            } catch (const Error& error) {
                storage_failure(error);
            } catch (...) {
                storage_failure(Error(ErrorCode::contract, Stage::frontend, "runtime_failure"));
            }
        }
        owner_done_ = true;
        job_.reset();
        reply_checkpoint(false);
        lock.unlock();
        changed_.notify_all();
    }

    void run_storage(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        while (changed_.wait(lock, thread_stop, [&] { return store_job_ || owner_done_; }) &&
               !thread_stop.stop_requested() && store_job_) {
            auto job = std::move(*store_job_);
            store_job_.reset();
            lock.unlock();
            std::optional<Error> error;
            std::shared_ptr<SessionState> next;
            try {
                const auto value = job.build();
                auto candidate = std::make_shared<SessionState>(*job.base);
                candidate->apply(value);
                next = std::move(candidate);
                if (journal_) journal_->append(value);
            }
            catch (const Error& failure) { error = failure; }
            catch (...) { error.emplace(ErrorCode::contract, Stage::frontend, "journal_failure"); }
            lock.lock();
            store_done_.emplace(StoreDone{job.commit, std::move(next), std::move(error)});
            changed_.notify_all();
        }
    }

    void run_stop_worker(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        while (changed_.wait(lock, thread_stop, [&] { return stop_signal_ || owner_done_; }) &&
               !thread_stop.stop_requested() && stop_signal_) {
            auto source = std::move(*stop_signal_);
            stop_signal_.reset();
            lock.unlock();
            source.request_stop();
            lock.lock();
            signalling_stop_ = false;
            changed_.notify_all();
        }
    }

    bool tool_checkpoint(const WorkIdentity& id, std::variant<ToolInvocation, ToolResult> value) {
        std::unique_lock lock(mutex_);
        if (owner_done_ || storage_failed_ || (std::holds_alternative<ToolInvocation>(value) && !can_dispatch(id))) return false;
        checkpoint_reply_.reset();
        checkpoint_.emplace(Checkpoint{id, std::move(value)});
        changed_.notify_all();
        changed_.wait(lock, [&] { return checkpoint_reply_.has_value() || owner_done_; });
        return !owner_done_ && checkpoint_reply_.value_or(false);
    }

    CompletedResponse generate(const Request& request, const Job& job) {
        {
            std::lock_guard lock(mutex_);
            require(can_dispatch(job.identity), "generation_cancelled", Stage::admission, ErrorCode::cancelled);
        }
        auto response = unwrap(generate_(request, job.stop));
        require(save_request(response.request()) == save_request(request) &&
                response.returned_model() == request.binding().returned_model,
                "provider_request_mismatch", Stage::admission);
        require(response.calls().size() <= Limits{}.calls && response.items().size() <= Limits{}.items,
                "runtime_response_limit", Stage::admission, ErrorCode::limit);
        require(!std::holds_alternative<Replay>(job.task) || response.calls().empty(),
                "tool_round_limit", Stage::admission);
        if (response.calls().empty()) (void)response_text(response);
        return response;
    }

    WorkerResult evaluate(Job& job) {
        try {
            if (const auto* generation = std::get_if<GenerateRequest>(&job.task)) return generate(generation->request, job);
            if (const auto* preparation = std::get_if<PrepareTools>(&job.task)) {
                OperationIds ids(preparation->operation_run);
                auto batch = unwrap(PreparedBatch::prepare(preparation->response, tools_, ids));
                return RunTools{preparation->response, std::move(batch), preparation->completed};
            }
            if (auto* tools = std::get_if<RunTools>(&job.task)) {
                const auto plan = tools->batch.plan();
                auto results = std::make_shared<std::vector<ToolResult>>();
                std::size_t index = tools->completed.size();
                *results = unwrap(tools->batch.execute(job.stop,
                    [&](std::string_view operation, std::string_view call) {
                        if (!tool_checkpoint(job.identity, plan.at(index))) return false;
                        std::lock_guard lock(mutex_);
                        if (!can_dispatch(job.identity)) return false;
                        inbox_.tool_starts.push_back({{std::string(operation)}, std::string(call)});
                        changed_.notify_all();
                        return true;
                    },
                    [&](const ToolResult& result) {
                        ++index;
                        return tool_checkpoint(job.identity, result);
                    }, tools->completed));
                return Replay{std::move(tools->response), std::move(results)};
            }
            if (const auto* replay = std::get_if<Replay>(&job.task))
                return generate(continuation(replay->response, *replay->results), job);
            const auto& request = std::get<CompactRequest>(job.task).request;
            {
                std::lock_guard lock(mutex_);
                require(can_dispatch(job.identity), "compaction_cancelled", Stage::admission, ErrorCode::cancelled);
            }
            require(bool(compact_), "compaction_unavailable", Stage::admission);
            auto compacted = unwrap(compact_(request, job.stop));
            require(save_request(compacted.request()) == save_request(request), "compaction_request_mismatch");
            require(compacted.output().size() + 1 < compact_item_threshold, "compaction_item_limit");
            require(compacted.output().size() < request.input().size() ||
                    Json::array(compacted.output()).dump().size() < Json::array(request.input()).dump().size(),
                    "compaction_no_progress", Stage::admission);
            return compacted;
        } catch (const Error& error) { return error; }
        catch (...) { return Error(ErrorCode::contract, Stage::admission, "worker_failure"); }
    }

    void run_worker(std::stop_token thread_stop) {
        std::unique_lock lock(mutex_);
        while (changed_.wait(lock, thread_stop, [&] { return owner_done_ || inbox_.aborting || (job_ && can_dispatch(job_->identity)); }) &&
               !thread_stop.stop_requested() && !owner_done_ && !inbox_.aborting) {
            auto job = std::move(*job_);
            job_.reset();
            lock.unlock();
            auto result = evaluate(job);
            lock.lock();
            if (owner_done_) break;
            inbox_.completion.emplace(Completion{std::move(job.identity), std::move(result)});
            changed_.notify_all();
        }
    }

public:
    Impl(SessionId session, ContextEpoch context, std::shared_ptr<const ToolSnapshot> tools,
         Generate generate, RuntimeLimits limits, SessionOptions options)
        : tools_(std::move(tools)), generate_(std::move(generate)), compact_(std::move(options.compact)), limits_(limits),
          profile_bytes_(Request({}, context, tools_ ? tools_->advertisement() : ToolAdvertisement{}, {}).json().dump().size()),
          state_(std::make_shared<SessionState>(session,
              Request({}, std::move(context), tools_ ? tools_->advertisement() : ToolAdvertisement{}, {}))) {
        require(valid_id(session.value) && tools_ && generate_ && limits.commands > 0 && limits.commands <= 256 &&
                limits.events >= 2 && limits.events <= 256 && limits.submit_ids > 0 && limits.submit_ids <= 65536 &&
                limits.compact_bytes >= 256 && limits.compact_bytes <= 4 * 1024 * 1024,
                "runtime_configuration", Stage::frontend, ErrorCode::contract);
        if (!options.directory.empty()) {
            journal_ = std::make_unique<Journal>(options.directory, [&](const Json& record) { state_->apply(record); },
                                                  std::move(options.journal));
            if (journal_->empty() && journal_->diagnostic().empty()) {
                const auto init = state_->initialization();
                journal_->append(init);
                state_->apply(init);
            }
            diagnostic_ = journal_->diagnostic();
            storage_failed_ = !diagnostic_.empty();
        } else state_->apply(state_->initialization());
        require(state_->submits.size() <= limits_.submit_ids, "session_command_limit");
        receipts_ = state_->submits;
        if (state_->turn) {
            phase_ = state_->turn->stage;
            for (const auto& result : state_->turn->results) {
                if (!result.dispatched) continue;
                auto output = std::make_shared<const std::string>(result.output);
                activity_.emplace_back(ToolFinished{{result.operation_id}, result.call_id, result.execution, result.effects, output});
            }
            if (phase_ != "completed" && phase_ != "cancelled") {
                phase_ = "paused";
                recovered_pause_ = true;
            }
            recovery_ = state_->unknown_operation();
            if (diagnostic_.empty()) diagnostic_ = state_->turn->diagnostic;
        }
        if (storage_failed_) phase_ = "paused";
        worker_ = std::jthread([this](std::stop_token stop) { run_worker(stop); });
        storage_worker_ = std::jthread([this](std::stop_token stop) { run_storage(stop); });
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
        storage_worker_.join();
        stop_worker_.join();
    }
    const RuntimeIncarnation& incarnation() const { return incarnation_; }

    Result<PostStatus> post(RuntimeCommand command) {
        return capture([&] {
            validate(command);
            if (command.incarnation != incarnation_) return PostStatus::stale_incarnation;
            std::lock_guard lock(mutex_);
            require(command.session == state_->session, "unknown_session", Stage::frontend, ErrorCode::contract);
            if (inbox_.input_closed || inbox_.aborting || owner_done_) return PostStatus::closed;
            if (const auto* cancel = std::get_if<Cancel>(&command.payload)) {
                if (!can_cancel() || request() != cancel->target || turn() != command.turn) return PostStatus::stale_target;
                if (inbox_.cancel || cancelling_) return PostStatus::duplicate;
                inbox_.cancel = std::move(command);
            } else if (const auto* resume = std::get_if<Resume>(&command.payload)) {
                if (phase_ != "paused" || request() != resume->target || turn() != command.turn) return PostStatus::stale_target;
                if (inbox_.resume) return PostStatus::queue_full;
                inbox_.resume = std::move(command);
            } else if (std::holds_alternative<GetSnapshot>(command.payload)) {
                if (inbox_.snapshot) return PostStatus::queue_full;
                inbox_.snapshot = std::move(command);
            } else {
                const auto& text = std::get<Submit>(command.payload).text;
                if (const auto found = receipts_.find(command.command.value); found != receipts_.end()) {
                    if (*found->second.text != text || found->second.turn != command.turn) return PostStatus::command_conflict;
                    if (inbox_.lookups.size() == limits_.commands) return PostStatus::queue_full;
                    inbox_.lookups.push_back(command.command);
                    changed_.notify_all();
                    return PostStatus::duplicate;
                }
                if (receipts_.size() == limits_.submit_ids) return PostStatus::command_id_limit;
                if (inbox_.submits.size() == limits_.commands) return PostStatus::queue_full;
                const auto id = receipts_.emplace(command.command.value,
                    SubmitReceipt{command.command, command.turn, {}, std::make_shared<const std::string>(text), "queued"}).first;
                try { inbox_.submits.push_back(std::move(command)); }
                catch (...) { receipts_.erase(id); throw; }
            }
            changed_.notify_all();
            return PostStatus::accepted;
        });
    }
    std::optional<RuntimeEvent> next_event(std::stop_token stop) {
        std::unique_lock lock(mutex_);
        changed_.wait(lock, stop, [&] { return owner_done_ || !events_.empty(); });
        if (events_.empty() || stop.stop_requested()) return {};
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
                 Generate generate, RuntimeLimits limits, SessionOptions options)
    : impl_(std::make_unique<Impl>(std::move(session), std::move(context), std::move(tools),
                                  std::move(generate), limits, std::move(options))) {}
Runtime::~Runtime() = default;
const RuntimeIncarnation& Runtime::incarnation() const { return impl_->incarnation(); }
Result<PostStatus> Runtime::post(RuntimeCommand command) { return impl_->post(std::move(command)); }
std::optional<RuntimeEvent> Runtime::next_event(std::stop_token stop) { return impl_->next_event(stop); }
void Runtime::close_input() { impl_->close_input(); }
} // namespace omh
