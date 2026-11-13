#include "omh/tools.hpp"
#include "omh/transport.hpp"
#include <curl/curl.h>
#include <algorithm>
#include <cctype>
#include <iostream>
#include <initializer_list>
#include <limits>
#include <optional>
#include <stdexcept>
#include <string>
#include <string_view>
#include <vector>

namespace {
using namespace omh;
std::size_t invocations = 0;
Json object(std::initializer_list<std::pair<std::string, Json>> fields) {
    return Json::object(std::vector<std::pair<std::string, Json>>(fields),20*1024*1024);
}
Json json_error(std::string diagnostic) {
    return object({{"schema_version",Json::parse("1")},{"code",Json::parse("0")},
        {"stage",Json::string("json")},{"diagnostic",Json::string(diagnostic)}});
}
unsigned char hex_digit(char c) {
    if(c>='0'&&c<='9')return static_cast<unsigned char>(c-'0');
    if(c>='a'&&c<='f')return static_cast<unsigned char>(c-'a'+10);
    if(c>='A'&&c<='F')return static_cast<unsigned char>(c-'A'+10);
    throw std::runtime_error("invalid hex");
}
std::string decode_hex(std::string_view hex){if(hex.size()%2)throw std::runtime_error("invalid hex");std::string out;out.reserve(hex.size()/2);for(std::size_t i=0;i<hex.size();i+=2)out.push_back(static_cast<char>((hex_digit(hex[i])<<4)|hex_digit(hex[i+1])));return out;}
std::vector<std::string> mime_values(const Json& row){std::vector<std::string> values;if(!row.has("mime"))return values;const auto mime=row.at("mime");if(mime.is_array()){for(const auto& value:mime.elements())values.push_back(value.text());}else values.push_back(mime.text());return values;}
Json json_probe(const Json& row) {
    try {
        std::string payload;
        if(row.has("payload_hex")) {
            const auto hex=row.at("payload_hex").text(); if(hex.size()%2)throw std::runtime_error("invalid hex");
            payload.reserve(hex.size()/2);for(std::size_t i=0;i<hex.size();i+=2)payload.push_back(static_cast<char>((hex_digit(hex[i])<<4)|hex_digit(hex[i+1])));
        } else payload=row.at("payload").text();
        std::size_t bytes=16*1024*1024,depth=64,nodes=131072;
        if(row.has("limits")){const auto limits=row.at("limits");if(limits.has("bytes"))bytes=limits.index("bytes");if(limits.has("depth"))depth=limits.index("depth");if(limits.has("nodes"))nodes=limits.index("nodes");}
        const auto value=Json::parse(payload,bytes,depth,nodes);
        std::vector<std::pair<std::string,Json>> fields={{"accepted",Json::parse("true")},{"dump",Json::string(value.dump(),20*1024*1024)},{"sha256",Json::string(sha256(value.dump()))}};
        if(row.has("other")){try{fields.emplace_back("equal",Json::parse(Json::parse(row.at("other").text())==value?"true":"false"));}catch(...){fields.emplace_back("equal",Json::parse("false"));}}
        if(row.has("index_key")){try{const auto key=row.at("index_key").text();const auto selected=value.at(std::string_view(key.data(),key.size()));fields.emplace_back("index",Json::parse(std::to_string(Json::object({{"value",selected}}).index("value"))));}catch(...){return object({{"accepted",Json::parse("false")},{"error",json_error("json_index")}});}}
        if(row.has("lifetime_probe")){try{const auto child=[](const Json& source){const auto temporary=source.dump();return Json::parse(temporary).at("a");}(value);const auto text=child.elements().at(0).text("text");fields.emplace_back("lifetime_text",Json::string(text));}catch(...){}}
        return Json::object(fields,20*1024*1024);
    } catch (...) { return object({{"accepted",Json::parse("false")},{"error",json_error("json_invalid")}}); }
}
Json provenance(const Request& request) {
    const auto& binding=request.binding();const auto& epoch=request.epoch();const auto& tools=request.tools();
    std::vector<Json> descriptors;for(const auto& d:tools.descriptors)descriptors.push_back(object({
        {"name",Json::string(d.name)},{"description",Json::string(d.description)},
        {"revision",Json::string(d.implementation_revision)},{"schema",d.schema}}));
    return object({{"provider",Json::string(binding.provider)},{"account",Json::string(binding.account)},
        {"requested_model",Json::string(binding.requested_model)},{"returned_model",Json::string(binding.returned_model)},
        {"epoch",object({{"id",Json::string(epoch.id)},{"instructions",Json::string(epoch.instructions)},{"exact_state",epoch.exact_state}})},
        {"tools",object({{"revision",Json::string(tools.revision)},{"descriptors",Json::array(descriptors)}})}});
}
ToolOutcome count(const Json& args, std::stop_token stop) {
    ++invocations;
    return test_value_tool().execute(args, stop);
}
struct LocalTransfer { HttpHeadParser parser; std::string body; bool failed=false; };
std::size_t receive_body(char* bytes,std::size_t size,std::size_t count,void* context) noexcept {
    auto& transfer=*static_cast<LocalTransfer*>(context);if(size&&count>std::numeric_limits<std::size_t>::max()/size)return 0;const auto n=size*count;
    try {
        require(transfer.parser.complete(), "protocol_network_head_incomplete");
        admit_http(transfer.parser.head(), responses_endpoint, "POST",
            Json::parse(R"({"stream":true,"store":false})"), MimePolicy::strict);
        require(n <= 16*1024*1024-transfer.body.size(), "protocol_network_body_limit");
        transfer.body.append(bytes,n);return n;
    } catch(...) {transfer.failed=true;return 0;}
}
std::size_t receive_header(char* bytes,std::size_t size,std::size_t count,void* context) noexcept {
    auto& transfer=*static_cast<LocalTransfer*>(context);if(size&&count>std::numeric_limits<std::size_t>::max()/size)return 0;const auto n=size*count;
    try {transfer.parser.line(std::string_view(bytes,n));return n;}
    catch(...) {transfer.failed=true;return 0;}
}
bool safe_loopback_url(const std::string& url) {
    CURLU* parsed=curl_url();if(!parsed)return false;
    const auto cleanup=std::unique_ptr<CURLU,decltype(&curl_url_cleanup)>(parsed,curl_url_cleanup);
    if(curl_url_set(parsed,CURLUPART_URL,url.c_str(),0)!=CURLUE_OK)return false;
    auto part=[&](CURLUPart key)->std::optional<std::string>{char* value=nullptr;if(curl_url_get(parsed,key,&value,0)!=CURLUE_OK)return std::nullopt;std::string result(value);curl_free(value);return result;};
    auto scheme=part(CURLUPART_SCHEME),host=part(CURLUPART_HOST),port=part(CURLUPART_PORT);
    if(!scheme||!host||!port||port->empty())return false;
    std::transform(scheme->begin(),scheme->end(),scheme->begin(),[](unsigned char c){return static_cast<char>(std::tolower(c));});
    std::transform(host->begin(),host->end(),host->begin(),[](unsigned char c){return static_cast<char>(std::tolower(c));});
    if(*scheme!="http"||!(*host=="127.0.0.1"||*host=="localhost"||*host=="[::1]"))return false;
    char* user=nullptr;const auto has_user=curl_url_get(parsed,CURLUPART_USER,&user,0)==CURLUE_OK;if(user)curl_free(user);
    char* password=nullptr;const auto has_password=curl_url_get(parsed,CURLUPART_PASSWORD,&password,0)==CURLUE_OK;if(password)curl_free(password);
    return !has_user&&!has_password;
}
Json process(const Json& row) {
    invocations = 0;
    if(row.has("op")&&row.at("op").text()=="json")return json_probe(row);
    ToolRegistry registry;
    auto tool = test_value_tool();
    if(row.has("op")&&row.at("op").text()=="admit_request"){
        const auto status=row.has("status")?static_cast<long>(std::stol(row.at("status").dump())):200L;
        auto profile=Json::object({{"stream",row.has("stream")?row.at("stream"):Json::parse("true")},{"store",row.has("store")?row.at("store"):Json::parse("false")}});
        bool accepted=false;try{admit_http({status,mime_values(row)},row.has("url")?row.at("url").text():"",row.has("method")?row.at("method").text():"POST",profile,MimePolicy::strict);accepted=true;}catch(...){}
        return object({{"accepted",Json::parse(accepted?"true":"false")}});
    }
    std::optional<std::size_t> invalid_call;
    std::vector<std::string> behaviors;
    if(row.has("batch")){const auto batch=row.at("batch");if(batch.has("invalid_call_index"))invalid_call=batch.index("invalid_call_index");if(batch.has("executor_behaviors"))for(const auto& behavior:batch.at("executor_behaviors").elements())behaviors.push_back(behavior.text());}
    std::size_t validations=0;
    tool.validate=[&](const Json&){return invalid_call!=validations++;};
    tool.execute=[&](const Json&,std::stop_token)->ToolOutcome{
        const auto index=invocations++;const auto behavior=index<behaviors.size()?behaviors[index]:"ok";
        if(behavior=="throw")throw std::runtime_error("synthetic executor failure");
        if(behavior=="oversized")return {Execution::succeeded,Effects::known_result,std::string(128*1024+1,'x')};
        if(behavior=="cancelled_unknown")return {Execution::failed,Effects::unknown,{}};
        if(behavior=="invalid_utf8")return {Execution::succeeded,Effects::known_result,std::string("\xff\xfe",2)};
        if(behavior=="inconsistent")return {Execution::succeeded,Effects::unknown,"{}"};
        return {Execution::succeeded,Effects::known_result,R"({"ok":true,"value":"OMH_FIXED_VALUE_42"})"};
    };
    registry.register_tool(std::move(tool));
    auto snapshot = registry.snapshot();
    Request request({}, {"epoch_1", "Exact fixture instructions",
        Json::parse(R"({"constraints":["offline","pure only"],"root":"fixture","revision":1})")},
        snapshot->advertisement(), {Json::parse(R"({"role":"developer","content":"fixture"})"),
        Json::parse(R"({"role":"user","content":"Call omh.get_test_value"})")});
    if(row.has("network_url")){
        const auto url=row.at("network_url").text();
        if(!safe_loopback_url(url))return object({{"accepted",Json::parse("false")},{"executors",Json::parse("0")}});
        CURL* curl=curl_easy_init();if(!curl)return object({{"accepted",Json::parse("false")},{"executors",Json::parse("0")}});
        LocalTransfer transfer;const auto payload=request.json().dump();
        curl_easy_setopt(curl,CURLOPT_URL,url.c_str());curl_easy_setopt(curl,CURLOPT_POST,1L);curl_easy_setopt(curl,CURLOPT_POSTFIELDS,payload.data());curl_easy_setopt(curl,CURLOPT_POSTFIELDSIZE_LARGE,static_cast<curl_off_t>(payload.size()));curl_easy_setopt(curl,CURLOPT_WRITEFUNCTION,receive_body);curl_easy_setopt(curl,CURLOPT_WRITEDATA,&transfer);curl_easy_setopt(curl,CURLOPT_HEADERFUNCTION,receive_header);curl_easy_setopt(curl,CURLOPT_HEADERDATA,&transfer);curl_easy_setopt(curl,CURLOPT_TIMEOUT_MS,3000L);curl_easy_setopt(curl,CURLOPT_NOSIGNAL,1L);curl_easy_setopt(curl,CURLOPT_FOLLOWLOCATION,0L);curl_easy_setopt(curl,CURLOPT_PROXY,"");curl_easy_setopt(curl,CURLOPT_NOPROXY,"*");curl_easy_setopt(curl,CURLOPT_PROTOCOLS_STR,"http");
        const auto status=curl_easy_perform(curl);curl_easy_cleanup(curl);
        if(status!=CURLE_OK||transfer.failed||!transfer.parser.complete())return object({{"accepted",Json::parse("false")},{"executors",Json::parse("0")}});
        auto decoded=decode_offline(request,transfer.parser.head(),{transfer.body});
        if(const auto* error=std::get_if<Error>(&decoded)){(void)error;return object({{"accepted",Json::parse("false")},{"executors",Json::parse("0")}});}
        const auto& response=std::get<CompletedResponse>(decoded);std::vector<Json> items=response.items();std::vector<Json> calls;
        for(const auto& call:response.calls())calls.push_back(object({{"id",Json::string(call.item_id)},{"call_id",Json::string(call.call_id)},{"name",Json::string(call.name)},{"arguments",Json::string(call.arguments)}}));
        OperationIds ids("local_http");auto prepared=PreparedBatch::prepare(response,snapshot,ids);
        if(const auto* error=std::get_if<Error>(&prepared)){(void)error;return object({{"accepted",Json::parse("true")},{"batch_accepted",Json::parse("false")},{"items",Json::array(items)},{"calls",Json::array(calls)},{"executors",Json::parse("0")}});}
        const auto results=std::get<PreparedBatch>(std::move(prepared)).execute();if(const auto* error=std::get_if<Error>(&results)){(void)error;return object({{"accepted",Json::parse("true")},{"batch_accepted",Json::parse("true")},{"execution_failed",Json::parse("true")},{"items",Json::array(items)},{"calls",Json::array(calls)},{"executors",Json::parse(std::to_string(invocations))}});}
        auto next=continuation(response,std::get<std::vector<ToolResult>>(results));return object({{"accepted",Json::parse("true")},{"batch_accepted",Json::parse("true")},{"items",Json::array(items)},{"calls",Json::array(calls)},{"continuation",next.json()},{"context_provenance",provenance(response.request())},{"executors",Json::parse(std::to_string(invocations))}});
    }
    const auto status = row.has("status") ? std::stol(row.at("status").dump()) : 0L;
    std::vector<std::string> chunks;
    if(row.has("chunks_hex")){for(const auto& chunk:row.at("chunks_hex").elements())chunks.push_back(decode_hex(chunk.text()));}
    else if(row.has("chunks")){for(const auto& chunk:row.at("chunks").elements())chunks.push_back(chunk.text());}
    else if(row.has("body_hex"))chunks.push_back(decode_hex(row.at("body_hex").text()));
    else chunks.push_back(row.has("body")?row.at("body").text():"");
    Limits limits;
    if(row.has("limits")){const auto l=row.at("limits");if(l.has("body"))limits.body=l.index("body");if(l.has("event"))limits.event=l.index("event");if(l.has("line"))limits.line=l.index("line");if(l.has("events"))limits.events=l.index("events");if(l.has("arguments"))limits.arguments=l.index("arguments");if(l.has("items"))limits.items=l.index("items");if(l.has("calls"))limits.calls=l.index("calls");if(l.has("json_depth"))limits.json_depth=l.index("json_depth");if(l.has("json_nodes"))limits.json_nodes=l.index("json_nodes");}
    std::vector<std::string> preview_texts;std::stop_source preview_stop;
    const bool preview=row.has("preview")&&row.at("preview")==Json::parse("true");
    const bool cancel_preview=row.has("cancel_preview_after_first")&&row.at("cancel_preview_after_first")==Json::parse("true");
    TextPreview callback;if(preview)callback=[&](std::string_view text){preview_texts.emplace_back(text);if(cancel_preview)preview_stop.request_stop();};
    auto decoded = decode_offline(request, HttpHead{status, mime_values(row)}, chunks, limits, MimePolicy::strict, preview_stop.get_token(),std::move(callback));
    if (const auto* error = std::get_if<Error>(&decoded)) {
        (void)error;
        auto result=object({{"accepted", Json::parse("false")}, {"executors", Json::parse("0")}});
        if(preview)return object({{"accepted",Json::parse("false")},{"executors",Json::parse("0")},{"preview_texts",Json::array([&]{std::vector<Json> v;for(const auto& t:preview_texts)v.push_back(Json::string(t));return v;}())}});
        return result;
    }
    const auto& response = std::get<CompletedResponse>(decoded);
    std::vector<Json> items = response.items();
    std::vector<Json> calls;
    for (const auto& call : response.calls()) calls.push_back(object({
        {"id", Json::string(call.item_id)}, {"call_id", Json::string(call.call_id)},
        {"name", Json::string(call.name)}, {"arguments", Json::string(call.arguments)}}));
    OperationIds operation_ids("parity");
    auto prepared = PreparedBatch::prepare(response, snapshot, operation_ids);
    if (std::holds_alternative<Error>(prepared)) return object({
        {"accepted", Json::parse("true")}, {"batch_accepted", Json::parse("false")},
        {"items", Json::array(items)}, {"calls", Json::array(calls)}, {"executors", Json::parse("0")}});
    std::stop_source batch_stop;
    if(row.has("batch")&&row.at("batch").has("cancel_before_dispatch")&&row.at("batch").at("cancel_before_dispatch")==Json::parse("true"))batch_stop.request_stop();
    auto results = std::get<PreparedBatch>(std::move(prepared)).execute(batch_stop.get_token());
    if(const auto* error=std::get_if<Error>(&results)){(void)error;return object({{"accepted",Json::parse("true")},{"batch_accepted",Json::parse("true")},{"execution_failed",Json::parse("true")},{"items",Json::array(items)},{"calls",Json::array(calls)},{"executors",Json::parse(std::to_string(invocations))}});}
    const auto& known=std::get<std::vector<ToolResult>>(results);const bool unknown=std::any_of(known.begin(),known.end(),[](const auto& r){return r.effects==Effects::unknown;});
    std::vector<std::pair<std::string,Json>> fields={{"accepted",Json::parse("true")},{"batch_accepted",Json::parse("true")},{"items",Json::array(items)},{"calls",Json::array(calls)},{"execution_failed",Json::parse(unknown?"true":"false")},{"context_provenance",provenance(response.request())},{"executors",Json::parse(std::to_string(invocations))}};
    if(!unknown)fields.emplace_back("continuation",continuation(response,known).json());
    if(preview){std::vector<Json> values;for(const auto& text:preview_texts)values.push_back(Json::string(text));fields.emplace_back("preview_texts",Json::array(values));}
    return Json::object(fields);
}
}

int main() {
    constexpr std::size_t max_line = 20 * 1024 * 1024;
    std::string line;
    char byte = 0;
    while (std::cin.get(byte)) {
        if (byte == '\n') {
            try { std::cout << process(omh::Json::parse(line,max_line,64,131072)).dump() << '\n'; }
            catch (...) { std::cout << R"({"adapter_error":true})" << '\n'; }
            line.clear();
        } else {
            if (line.size() == max_line) {
                std::cerr << "protocol_adapter: input line exceeds limit\n";
                return 2;
            }
            line.push_back(byte);
        }
    }
    if (!line.empty()) { std::cerr << "protocol_adapter: unterminated input line\n"; return 2; }
}
