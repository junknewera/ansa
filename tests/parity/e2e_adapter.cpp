// Synthetic composition over production config/tools/runtime/journal and wire codec.
#define main ansa_session_adapter_unused_main
#include "session_adapter.cpp"
#undef main
#include "../../src/cli.cpp"
#include "../../src/agent_cli.cpp"
#include "omh/local_tools.hpp"
#include "omh/local_storage.hpp"
#include "omh/offline.hpp"
#include <fstream>
#include <mutex>
#include <atomic>
namespace {
using BenchClock=std::chrono::steady_clock;
struct Measurements{std::mutex mutex;std::optional<BenchClock::time_point> sync_start,terminal;std::vector<Json> fsync,done;Json save(){std::lock_guard lock(mutex);return Json::object({{"fsync_ns",Json::array(fsync)},{"terminal_durable_ns",Json::array(done)}});}};
std::size_t elapsed_ns(BenchClock::time_point start){return static_cast<std::size_t>(std::chrono::duration_cast<std::chrono::nanoseconds>(BenchClock::now()-start).count());}
bool flag(const Json& row,const char* key){return row.has(key)&&row.at(key)==boolean(true);}
local::Fd fixture_child(int parent,const char* name){require(::mkdirat(parent,name,0700)==0||errno==EEXIST,"fixture_directory");local::Fd fd(::openat(parent,name,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC));struct stat meta{};require(fd.get()>=0&&::fstat(fd.get(),&meta)==0&&meta.st_uid==::geteuid()&&(meta.st_mode&077)==0,"fixture_directory");return fd;}
void fixture_files(const local::Fd& cwd,bool fresh_process=false){
    if(fresh_process)for(const auto* name:{"primary","ready"}){struct stat meta{};require(::fstatat(cwd.get(),name,&meta,AT_SYMLINK_NOFOLLOW)<0&&errno==ENOENT,"fixture_readiness_exists");}
    local::Fd created(::openat(cwd.get(),"value.txt",O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0600));
    if(created.get()>=0){local::write_all(created.get(),"SOURCE_VALUE\n");local::sync(created.get());}else require(errno==EEXIST,"fixture_file");
    for(const auto* name:{"value.txt","marker"}){local::Fd file(::openat(cwd.get(),name,O_RDONLY|O_NONBLOCK|O_NOFOLLOW|O_CLOEXEC));if(file.get()<0&&errno==ENOENT&&std::string_view(name)=="marker")continue;struct stat meta{};require(file.get()>=0&&::fstat(file.get(),&meta)==0&&S_ISREG(meta.st_mode)&&meta.st_nlink==1&&meta.st_uid==::geteuid()&&meta.st_size<=4096,"fixture_file");std::string bytes;char buffer[4097];const auto size=::read(file.get(),buffer,sizeof(buffer));require(size>=0&&size<=4096,"fixture_data");bytes.assign(buffer,static_cast<std::size_t>(size));require(std::string_view(name)=="value.txt"?bytes=="SOURCE_VALUE\n":bytes.find_first_not_of('x')==std::string::npos,"fixture_data");}
}
void fixture_workspace(const std::filesystem::path& root){
    auto parent=local::open_private_directory(root);auto home=fixture_child(parent.get(),"home");auto cwd=fixture_child(home.get(),"workspace");fixture_child(parent.get(),"journal");fixture_child(parent.get(),"auth");fixture_files(cwd);
}
Json sse_bench(const std::filesystem::path& root,const Json& row,const Request& request){
    auto directory=local::open_private_directory(root);local::Fd file(::openat(directory.get(),"wire.sse",O_RDONLY|O_NONBLOCK|O_NOFOLLOW|O_CLOEXEC));struct stat meta{};
    require(file.get()>=0&&::fstat(file.get(),&meta)==0&&S_ISREG(meta.st_mode)&&meta.st_nlink==1&&meta.st_uid==::geteuid()&&(meta.st_mode&077)==0&&meta.st_size>0&&meta.st_size<=2*1024*1024,"fixture_wire");
    std::string wire;char buffer[8192];for(;;){const auto size=::read(file.get(),buffer,sizeof(buffer));if(size<0&&errno==EINTR)continue;require(size>=0&&static_cast<std::size_t>(size)<=2*1024*1024-wire.size(),"fixture_wire");if(!size)break;wire.append(buffer,static_cast<std::size_t>(size));}
    const auto iterations=row.index("iterations");require(iterations>=1&&iterations<=1000,"fixture_iterations");const std::string opaque(256*1024,'x');const auto begin=BenchClock::now();
    for(std::size_t i=0;i<iterations;++i){ResponseStream stream(request);for(std::size_t offset=0;offset<wire.size();offset+=4096)stream.feed(std::string_view(wire).substr(offset,std::min<std::size_t>(4096,wire.size()-offset)));const auto response=unwrap(stream.finish());require(response.returned_model()==request.binding().returned_model&&response.items().size()==2&&response.calls().empty()&&response.items()[0].text("encrypted_content")==opaque,"fixture_sse_admission");}
    const auto elapsed=elapsed_ns(begin);
    return Json::object({{"wire_sha256",str(sha256(wire))},{"bytes_per_response",num(wire.size())},{"iterations",num(iterations)},{"admissions",num(iterations)},{"tool_calls",num(0)},{"elapsed_ns",num(elapsed)}});
}
Json e2e_run(const Json& row){
    const auto root=fixture_root(row),home=root/"home",cwd=home/"workspace";
    fixture_workspace(root);
    const auto scope=row.has("scope")?row.text("scope"):std::string{};require(scope.empty()||(scope.starts_with("metrics_")&&valid_id(scope)),"fixture_scope");
    const auto journal_dir=root/(scope.empty()?"journal":scope),tool_cwd=scope.empty()?cwd:cwd/scope;const auto path_prefix=scope.empty()?std::string{}:scope+"/";
    if(!scope.empty()){auto parent=local::open_private_directory(cwd);local::Fd scoped(::openat(parent.get(),scope.c_str(),O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC));struct stat meta{};require(scoped.get()>=0&&::fstat(scoped.get(),&meta)==0&&meta.st_uid==::geteuid()&&(meta.st_mode&077)==0,"fixture_scope_directory");fixture_files(scoped,true);}
    const auto settings=load_agent_settings(cwd,home);const auto epoch=agent_epoch(settings);const auto token=sha256(epoch.exact_state.at("instructions").dump());
    const auto tools=local_tools(settings,epoch,journal_dir,root/"auth");
    if(row.has("op")&&row.text("op")=="sse_bench")return sse_bench(root,row,Request({},epoch,tools->advertisement(),{}));
    const bool quiet=flag(row,"quiet"),plan=row.text("mode")=="plan";
    const auto turns=row.index("turns"),start=row.index("start");require(turns<=(quiet?4096u:8u)&&start<=4096-turns,"fixture_turn_limit");
    std::vector<Json> requests,events;std::mutex mutex;std::atomic<std::size_t> generations=0,summaries=0;
    const bool measuring=flag(row,"interactive"),cancel_process=flag(row,"cancel_process");Measurements measurements;
    StreamingGenerate generate=[&](const Request& request,std::stop_token stop,TextPreview preview)->Result<CompletedResponse>{
        require(!stop.stop_requested(),"fixture_cancelled");++generations;if(!quiet){std::lock_guard lock(mutex);requests.push_back(save_request(request));}
        const auto& input=request.input();std::size_t first=input.size();while(first>0){--first;if(input[first].has("role")&&input[first].text("role")=="user")break;}
        require(first<input.size()&&input[first].has("role"),"fixture_user");const auto label=input[first].text("content");
        std::vector<Json> outputs;for(std::size_t i=first+1;i<input.size();++i)if(input[i].has("type")&&input[i].text("type")=="function_call_output")outputs.push_back(input[i]);
        const auto index=outputs.size();Json item=Json::parse("null");
        if(index==(plan?1u:3u)){
            if(!plan)require(Json::parse(outputs[1].text("output")).index("exit_code")==7,"fixture_known_failure_lost");
            if(preview)preview("E2E_PREVIEW");
            if(measuring){const auto deadline=BenchClock::now()+std::chrono::milliseconds(800);while(BenchClock::now()<deadline){require(!stop.stop_requested(),"fixture_cancelled");std::this_thread::sleep_for(std::chrono::milliseconds(1));}}
            item=Json::object({{"type",str("message")},{"id",str("message_"+label)},{"role",str("assistant")},{"status",str("completed")},{"content",Json::array({Json::object({{"type",str("output_text")},{"text",str(plan?"PLAN_OK":"E2E_OK:exit7")}})})}});
        }else{
            const auto name=index==1?"run_command":"read_file";
            const auto args=index==1?Json::object({{"argv",Json::array({str("sh"),str("-c"),str(cancel_process?"printf x >> marker; printf '%s' $$ > primary; printf ready > ready; exec sleep 30":"printf x >> marker; printf RUN_OK; exit 7")})},{"cwd",str(tool_cwd.string())},{"timeout_ms",num(cancel_process?30000:3000)},{"instruction_token",str(token)}}):
                Json::object({{"path",str(path_prefix+(index==0?"value.txt":"marker"))},{"offset",num(0)},{"max_bytes",num(8192)},{"instruction_token",str(token)}});
            item=Json::object({{"type",str("function_call")},{"id",str("item_"+label+"_"+std::to_string(index))},{"call_id",str("call_"+label+"_"+std::to_string(index))},{"name",str(name)},{"namespace",str("omh")},{"arguments",str(args.dump())},{"status",str("completed")}});
        }
        const auto response=load_response(Json::object({{"request",save_request(request)},{"id",str("response_"+label+"_"+std::to_string(index))},{"model",str(request.binding().returned_model)},{"items",Json::array({item})}}));
        if(measuring&&index==(plan?1u:3u)){std::lock_guard lock(measurements.mutex);measurements.terminal=BenchClock::now();}return response;
    };
    SessionOptions options;options.directory=journal_dir;options.binding={};options.keep_recent_turns=2;options.stream_generate=generate;
    std::size_t fault_hits=0;
    if(row.has("fault")||measuring){options.journal.observe=[&](JournalPoint point,std::string_view kind){
        if(measuring){std::lock_guard lock(measurements.mutex);if(point==JournalPoint::before_sync)measurements.sync_start=BenchClock::now();else if(point==JournalPoint::after_sync){if(measurements.sync_start){measurements.fsync.push_back(num(elapsed_ns(*measurements.sync_start)));measurements.sync_start.reset();}if(kind=="done"){require(measurements.terminal.has_value(),"fixture_terminal_time");measurements.done.push_back(num(elapsed_ns(*measurements.terminal)));}}}
        if(row.has("fault")&&point==JournalPoint::after_sync&&kind==row.at("fault").text("kind")&&++fault_hits==row.at("fault").index("occurrence"))::_exit(71);
    };}
    options.compact=[&](const Request& request,std::stop_token stop){++summaries;return offline_compact(request,stop);};
    RuntimeLimits limits;limits.tool_rounds=flag(row,"limited")?1:8;limits.compact_bytes=flag(row,"compact")?256:256*1024;
    Json last=Json::parse("null");std::size_t resumes=0;
    {
        Runtime runtime({"e2e"},epoch,tools,[&](const Request& request,std::stop_token stop){return generate(request,stop,{});},limits,options);
        if(measuring){const auto code=text_frontend(runtime,{"e2e"},std::nullopt,"M8 fixture");return Json::object({{"exit",num(static_cast<std::size_t>(code))},{"metrics",measurements.save()},{"generation_count",num(generations)},{"summary_count",num(summaries)}});}
        std::string active_turn;
        auto post=[&](std::string id,CommandPayload payload){require(unwrap(runtime.post({1,{"e2e"},{active_turn},runtime.incarnation(),{id},std::move(payload)}))==PostStatus::accepted,"fixture_post");};
        auto consume=[&](const RuntimeEvent& e){if(std::holds_alternative<FrontendSnapshot>(e.payload))last=encode_event(e);if(!quiet)events.push_back(encode_event(e));};
        for(std::size_t index=start;index<start+turns;++index){
            active_turn="turn"+std::to_string(index);const auto id=std::string(flag(row,"resume")?"restart":"command")+std::to_string(index);
            if(flag(row,"resume")){
                bool posted=false;while(const auto event=runtime.next_event()){consume(*event);if(const auto* snapshot=std::get_if<FrontendSnapshot>(&event->payload)){require(snapshot->request.has_value(),"fixture_resume");post(id,Resume{*snapshot->request});posted=true;break;}}require(posted,"fixture_resume_snapshot");
            }else post(id,Submit{"fixture"+std::to_string(index),plan?AgentMode::plan:AgentMode::basic});
            bool done=false;while(const auto event=runtime.next_event()){
                consume(*event);
                if(const auto* error=std::get_if<RuntimeError>(&event->payload))require(error->diagnostic=="tool_round_limit","fixture_runtime_error");
                if(const auto* snapshot=std::get_if<FrontendSnapshot>(&event->payload);snapshot&&snapshot->resume_available&&snapshot->state=="paused"){
                    if(flag(row,"pause")){done=true;break;}require(++resumes<=8*turns,"fixture_resume_limit");post("resume"+std::to_string(index)+"_"+std::to_string(resumes),Resume{*snapshot->request});
                }
                if(std::holds_alternative<Done>(event->payload)){done=true;break;}
            }require(done,"fixture_done");
        }
        runtime.close_input();while(const auto event=runtime.next_event())consume(*event);
    }
    return Json::object({{"events",Json::array(events)},{"requests",Json::array(requests)},{"snapshot",last},{"generation_count",num(generations)},{"summary_count",num(summaries)},{"resume_count",num(resumes)},{"plan",boolean(plan)}});
}
}
int main(int argc,char** argv){if(argc==3&&std::string_view(argv[1])=="--process-worker")return process_worker(argv[2]);
    if((argc==4||argc==5)&&std::string_view(argv[1])=="--e2e-frontend"){try{const auto mode=std::string_view(argv[3]);require(mode=="turn"||mode=="cancel","fixture_mode");std::vector<std::pair<std::string,Json>> fields{{"root",str(argv[2])},{"mode",str("basic")},{"turns",num(0)},{"start",num(0)},{"quiet",boolean(true)},{"interactive",boolean(true)},{"cancel_process",boolean(mode=="cancel")}};if(argc==5)fields.emplace_back("scope",str(argv[4]));const auto result=e2e_run(Json::object(fields));std::cout<<"\nM8_METRICS "<<result.dump()<<std::endl;return static_cast<int>(result.index("exit"));}catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}
    std::string line;while(std::getline(std::cin,line)){try{std::cout<<Json::object({{"accepted",boolean(true)},{"value",e2e_run(Json::parse(line))}}).dump()<<'\n';}catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}return 0;}
