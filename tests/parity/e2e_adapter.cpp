// Synthetic composition over production config/tools/runtime/journal and wire codec.
#define main ansa_session_adapter_unused_main
#include "session_adapter.cpp"
#undef main
#include "../../src/cli.cpp"
#include "omh/local_tools.hpp"
#include "omh/local_storage.hpp"
#include "omh/offline.hpp"
#include <fstream>
#include <mutex>
#include <atomic>
namespace {
bool flag(const Json& row,const char* key){return row.has(key)&&row.at(key)==boolean(true);}
local::Fd fixture_child(int parent,const char* name){require(::mkdirat(parent,name,0700)==0||errno==EEXIST,"fixture_directory");local::Fd fd(::openat(parent,name,O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC));struct stat meta{};require(fd.get()>=0&&::fstat(fd.get(),&meta)==0&&meta.st_uid==::geteuid()&&(meta.st_mode&077)==0,"fixture_directory");return fd;}
void fixture_workspace(const std::filesystem::path& root){
    auto parent=local::open_private_directory(root);auto home=fixture_child(parent.get(),"home");auto cwd=fixture_child(home.get(),"workspace");fixture_child(parent.get(),"journal");fixture_child(parent.get(),"auth");
    local::Fd created(::openat(cwd.get(),"value.txt",O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW|O_CLOEXEC,0600));
    if(created.get()>=0){local::write_all(created.get(),"SOURCE_VALUE\n");local::sync(created.get());}else require(errno==EEXIST,"fixture_file");
    for(const auto* name:{"value.txt","marker"}){local::Fd file(::openat(cwd.get(),name,O_RDONLY|O_NONBLOCK|O_NOFOLLOW|O_CLOEXEC));if(file.get()<0&&errno==ENOENT&&std::string_view(name)=="marker")continue;struct stat meta{};require(file.get()>=0&&::fstat(file.get(),&meta)==0&&S_ISREG(meta.st_mode)&&meta.st_nlink==1&&meta.st_uid==::geteuid()&&meta.st_size<=4096,"fixture_file");std::string bytes;char buffer[4097];const auto size=::read(file.get(),buffer,sizeof(buffer));require(size>=0&&size<=4096,"fixture_data");bytes.assign(buffer,static_cast<std::size_t>(size));require(std::string_view(name)=="value.txt"?bytes=="SOURCE_VALUE\n":bytes.find_first_not_of('x')==std::string::npos,"fixture_data");}
}
Json e2e_run(const Json& row){
    const auto root=fixture_root(row),home=root/"home",cwd=home/"workspace";
    fixture_workspace(root);
    const auto settings=load_agent_settings(cwd,home);const auto epoch=agent_epoch(settings);const auto token=sha256(epoch.exact_state.at("instructions").dump());
    const auto tools=local_tools(settings,epoch,root/"journal",root/"auth");
    const bool quiet=flag(row,"quiet"),plan=row.text("mode")=="plan";
    const auto turns=row.index("turns"),start=row.index("start");require(turns<=(quiet?4096u:8u)&&start<=4096-turns,"fixture_turn_limit");
    std::vector<Json> requests,events;std::mutex mutex;std::atomic<std::size_t> generations=0,summaries=0;
    StreamingGenerate generate=[&](const Request& request,std::stop_token stop,TextPreview preview)->Result<CompletedResponse>{
        require(!stop.stop_requested(),"fixture_cancelled");++generations;if(!quiet){std::lock_guard lock(mutex);requests.push_back(save_request(request));}
        const auto& input=request.input();std::size_t first=input.size();while(first>0){--first;if(input[first].has("role")&&input[first].text("role")=="user")break;}
        require(first<input.size()&&input[first].has("role"),"fixture_user");const auto label=input[first].text("content");
        std::vector<Json> outputs;for(std::size_t i=first+1;i<input.size();++i)if(input[i].has("type")&&input[i].text("type")=="function_call_output")outputs.push_back(input[i]);
        const auto index=outputs.size();Json item=Json::parse("null");
        if(index==(plan?1u:3u)){
            if(!plan)require(Json::parse(outputs[1].text("output")).index("exit_code")==7,"fixture_known_failure_lost");
            if(preview)preview("E2E_PREVIEW");
            item=Json::object({{"type",str("message")},{"id",str("message_"+label)},{"role",str("assistant")},{"status",str("completed")},{"content",Json::array({Json::object({{"type",str("output_text")},{"text",str(plan?"PLAN_OK":"E2E_OK:exit7")}})})}});
        }else{
            const auto name=index==1?"run_command":"read_file";
            const auto args=index==1?Json::object({{"argv",Json::array({str("sh"),str("-c"),str("printf x >> marker; printf RUN_OK; exit 7")})},{"cwd",str(cwd.string())},{"timeout_ms",num(3000)},{"instruction_token",str(token)}}):
                Json::object({{"path",str(index==0?"value.txt":"marker")},{"offset",num(0)},{"max_bytes",num(8192)},{"instruction_token",str(token)}});
            item=Json::object({{"type",str("function_call")},{"id",str("item_"+label+"_"+std::to_string(index))},{"call_id",str("call_"+label+"_"+std::to_string(index))},{"name",str(name)},{"namespace",str("omh")},{"arguments",str(args.dump())},{"status",str("completed")}});
        }
        return load_response(Json::object({{"request",save_request(request)},{"id",str("response_"+label+"_"+std::to_string(index))},{"model",str(request.binding().returned_model)},{"items",Json::array({item})}}));
    };
    SessionOptions options;options.directory=root/"journal";options.binding={};options.keep_recent_turns=2;options.stream_generate=generate;
    std::size_t fault_hits=0;
    if(row.has("fault")){const auto fault=row.at("fault");options.journal.observe=[&,fault](JournalPoint point,std::string_view kind){if(point==JournalPoint::after_sync&&kind==fault.text("kind")&&++fault_hits==fault.index("occurrence"))::_exit(71);};}
    options.compact=[&](const Request& request,std::stop_token stop){++summaries;return offline_compact(request,stop);};
    RuntimeLimits limits;limits.tool_rounds=flag(row,"limited")?1:8;limits.compact_bytes=flag(row,"compact")?256:256*1024;
    Json last=Json::parse("null");std::size_t resumes=0;
    {
        Runtime runtime({"e2e"},epoch,tools,[&](const Request& request,std::stop_token stop){return generate(request,stop,{});},limits,options);
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
int main(int argc,char** argv){if(argc==3&&std::string_view(argv[1])=="--process-worker")return process_worker(argv[2]);std::string line;while(std::getline(std::cin,line)){try{std::cout<<Json::object({{"accepted",boolean(true)},{"value",e2e_run(Json::parse(line))}}).dump()<<'\n';}catch(const std::exception& e){std::cerr<<e.what()<<'\n';return 1;}}return 0;}
