// Test-only synthetic session adapter. Every codec/reducer is production code.
#include "omh/session.hpp"
#include "omh/journal.hpp"
#include "omh/agent_config.hpp"
#include "omh/offline.hpp"
#include <algorithm>
#include <chrono>
#include <iostream>
#include <thread>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>
using namespace omh;
namespace {
Json str(std::string_view v) { return Json::string(v); }
Json num(std::size_t v) { return Json::parse(std::to_string(v)); }
Json boolean(bool v) { return Json::parse(v?"true":"false"); }
template<class T> T unwrap(Result<T> value) { if (const auto* e=std::get_if<Error>(&value)) throw *e; return std::get<T>(std::move(value)); }
Json receipt(const SubmitReceipt& r) { return Json::object({{"command",str(r.command.value)},{"turn",str(r.turn.value)},{"request",str(r.request.value)},{"text",str(*r.text)},{"state",str(r.state)},{"mode",str(r.mode==AgentMode::plan?"plan":"basic")}}); }
Json snapshot(const SessionState& s) {
    std::vector<Json> submits; for(const auto& [_,r]:s.submits)submits.push_back(receipt(r));
    Json turn=Json::parse("null");
    if(s.turn) {const auto& t=*s.turn;std::vector<Json> results,previous,resumes;
        for(const auto& r:t.results)results.push_back(save_result(r));for(const auto& r:t.previous_results)previous.push_back(save_result(r));for(const auto& r:t.resumes)resumes.push_back(str(r));
        turn=Json::object({{"receipt",receipt(t.receipt)},{"run",str(t.operation_run)},{"next",save_request(t.next)},
            {"response",t.response?save_response(*t.response):Json::parse("null")},{"tools_response",t.tools_response?save_response(*t.tools_response):Json::parse("null")},
            {"results",Json::array(results)},{"previous_results",Json::array(previous)},{"intent",t.intent?save_invocation(*t.intent):Json::parse("null")},
            {"stage",str(t.stage)},{"diagnostic",str(t.diagnostic)},{"paused",boolean(t.paused)},{"resumes",Json::array(resumes)},
            {"rounds",num(t.rounds)},{"operation_offset",num(t.operation_offset)},{"round_limit",num(t.round_limit)},{"call_limit",num(t.call_limit)},{"resume_round",num(t.resume_round)},{"resume_operation",num(t.resume_operation)}});
    }
    const auto request=Request(s.configuration.binding(),s.epoch,s.configuration.tools(),s.history);
    return Json::object({{"session",str(s.session.value)},{"initialized",boolean(s.initialized)},{"configuration",save_request(s.configuration)},{"context",save_request(request)},
        {"history",Json::array(s.history)},{"history_bytes",num(s.history_bytes)},{"next_request",num(s.next_request)},{"compactions",num(s.compactions)},
        {"submits",Json::array(submits)},{"turn",turn},{"unknown",s.unknown_operation()?str(s.unknown_operation()->value):Json::parse("null")}});
}
Json page(const SessionState& state,const Json& query) { const GetTranscript q{query.index("before"),query.index("limit"),query.has("forward")&&query.at("forward")==Json::parse("true")}; const auto p=state.transcript_page(q);std::vector<Json> entries;
    for(const auto& e:p.entries)entries.push_back(Json::object({{"turn",str(e.turn.value)},{"request",str(e.request.value)},{"role",str(e.role)},{"text",str(e.text)},{"state",str(e.state)},{"mode",str(e.mode==AgentMode::plan?"plan":"basic")}}));
    return Json::object({{"begin",num(p.begin)},{"end",num(p.end)},{"total",num(p.total)},{"entries",Json::array(entries)}});
}
SessionState state_for(const Json& row) {const auto config=load_request(row.at("configuration"));return SessionState({row.text("session")},config,row.has("plan_tools")?load_request(row.at("plan_tools")).tools():ToolAdvertisement{});}
std::filesystem::path fixture_root(const Json& row) {
    const auto text=row.text("root");const std::filesystem::path path(text);
    require(text.find('\0')==std::string::npos && path.is_absolute() && path.lexically_normal()==path && path.parent_path()==std::filesystem::temp_directory_path() && path.filename().string().starts_with("ansa-m4-"),"fixture_root");
    const auto fd=::open(path.c_str(),O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC);struct stat meta{};
    const bool valid=fd>=0 && ::fstat(fd,&meta)==0 && meta.st_uid==::geteuid() && (meta.st_mode&077)==0;
    const auto marker=valid?::openat(fd,"fixture-owner",O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC):-1;char bytes[32]{};
    const bool safe=marker>=0 && ::fstat(marker,&meta)==0 && S_ISREG(meta.st_mode) && meta.st_uid==::geteuid() && meta.st_nlink==1 && (meta.st_mode&077)==0 && meta.st_size==21;
    const auto n=safe?::read(marker,bytes,sizeof(bytes)):-1;
    const bool owned=safe && std::string_view(bytes,n<0?0:static_cast<std::size_t>(n))=="ansa-m4-synthetic-v1\n";
    if(marker>=0)::close(marker);if(fd>=0)::close(fd);require(valid&&owned,"fixture_owner");return path;
}
JournalOptions options(const Json& row) { if(!row.has("fault"))return {}; const auto f=row.at("fault");const auto kind=f.text("kind"),point=f.text("point"),action=f.text("action");
    return {[=](JournalPoint p,std::string_view k) {const char* text=p==JournalPoint::before_write?"before_write":p==JournalPoint::after_write?"after_write":p==JournalPoint::before_sync?"before_sync":"after_sync";
        if(k==kind && point==text) {if(action=="exit")::_exit(71);fail(ErrorCode::contract,Stage::frontend,"journal_injected_io");}}}; }
Json run(const Json& row) {
    const auto op=row.text("op");
    if(op=="request")return save_request(load_request(row.at("value")));
    if(op=="response")return save_response(load_response(row.at("value")));
    if(op=="result")return save_result(load_result(row.at("value")));
    if(op=="summary_request")return save_request(summary_request(load_request(row.at("value"))));
    if(op=="compact") {const auto r=load_request(row.at("request"));const auto c=unwrap(CompactedContext::admit(r,row.at("native")));const auto e=compact_epoch(r.epoch(),row.has("generation")?row.index("generation"):1,c);return Json::object({{"native",c.native()},{"epoch",save_request(Request(r.binding(),e,r.tools(),c.output())).at("epoch")}});}
    if(op=="summary") {const auto c=unwrap(CompactedContext::from_summary(load_request(row.at("request")),load_response(row.at("response"))));return c.native();}
    if(op=="answer") {const auto request=load_request(row.at("request"));return save_response(load_response(Json::object({{"request",save_request(request)},{"id",str("saved_answer")},{"model",str(request.binding().returned_model)},
        {"items",Json::array({Json::object({{"id",str("message_saved")},{"type",str("message")},{"role",str("assistant")},{"status",str("completed")},{"content",Json::array({Json::object({{"type",str("output_text")},{"text",row.at("text")}})})}})})}})));}
    if(op=="offline")return save_response(unwrap(offline_generate(load_request(row.at("request")),{})));
    if(op=="state") {auto state=state_for(row);std::vector<Json> steps,pages;for(const auto& event:row.at("events").elements()) {try{state.apply(event);steps.push_back(Json::object({{"accepted",boolean(true)},{"state",snapshot(state)}}));}catch(const Error& e){steps.push_back(Json::object({{"accepted",boolean(false)},{"diagnostic",str(e.diagnostic)},{"state",snapshot(state)}}));}}
        if(row.has("queries"))for(const auto& q:row.at("queries").elements())pages.push_back(page(state,q));return Json::object({{"steps",Json::array(steps)},{"pages",Json::array(pages)},{"state",snapshot(state)}});}
    if(op=="catalog") {const auto root=fixture_root(row);AgentSettings settings;settings.directory=root;settings.cwd=root/"workspace";const auto selected=select_session(settings,row.has("resume")&&row.at("resume")==Json::parse("true"),row.has("id")?row.text("id"):"","offline");return Json::object({{"id",str(selected.id.value)},{"directory",str(selected.directory.string())}});}
    if(op=="journal" || op=="store" || op=="hold") {const auto root=fixture_root(row);std::vector<Json> events,results;auto state=op=="store"?std::optional<SessionState>(state_for(row)):std::nullopt;
        Journal journal(root/"journal",[&](const Json& event){if(state)state->apply(event);events.push_back(event);},options(row));
        if(op=="hold") {std::cout<<"{\"ready\":true}"<<std::endl;std::this_thread::sleep_for(std::chrono::milliseconds(500));}
        if(op=="store" && journal.diagnostic().empty() && journal.empty()) {const auto init=state->initialization();journal.append(init);state->apply(init);events.push_back(init);}
        std::string diagnostic=journal.diagnostic();
        if(row.has("events"))for(const auto& event:row.at("events").elements()) {try {if(state) {require(diagnostic.empty(),"journal_read_only");auto next=*state;next.apply(event);journal.append(event);*state=std::move(next);}else journal.append(event);results.push_back(Json::object({{"accepted",boolean(true)}}));}catch(const Error& e){if(state && e.diagnostic.starts_with("journal_"))diagnostic=e.diagnostic;results.push_back(Json::object({{"accepted",boolean(false)},{"diagnostic",str(e.diagnostic)}}));}}
        return Json::object({{"events",Json::array(events)},{"appends",Json::array(results)},{"diagnostic",str(diagnostic)},{"empty",boolean(journal.empty())},{"state",state?snapshot(*state):Json::parse("null")}});
    }
    fail(ErrorCode::contract,Stage::frontend,"session_adapter_operation");
}
}
int main() {std::string line;char c;while(std::cin.get(c)){if(c!='\n'){if(line.size()==4*1024*1024)return 2;line+=c;continue;}try{std::cout<<Json::object({{"accepted",boolean(true)},{"value",run(Json::parse(line,4*1024*1024))}}).dump()<<'\n';}catch(const Error& e){std::cout<<Json::object({{"accepted",boolean(false)},{"diagnostic",str(e.diagnostic)}}).dump()<<'\n';}catch(...){std::cout<<"{\"accepted\":false,\"diagnostic\":\"session_adapter_error\"}\n";}line.clear();}return line.empty()?0:2;}
