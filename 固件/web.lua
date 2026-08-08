local M = {}

local function default_text_or(value, fallback)
  if value == nil then return fallback or "" end
  local text = tostring(value)
  if text == "" then return fallback or "" end
  return text
end

function M.new(ctx)
  local APP = ctx.app
  local APP_DIR = ctx.app_dir
  local config = ctx.config
  local JSON = ctx.json
  local text_or = ctx.text_or or default_text_or

  local function app_asset_url(name)
    local base = tostring(APP_DIR or ""):gsub("\\", "/")
    if base:sub(1, 4) == "/sd/" then base = base:sub(4) end
    if base:sub(1, 1) ~= "/" then base = "/apps/holocubic-smtc-music" end
    return base .. "/" .. name
  end

  local self = {
    routes = {},
    route_base = "",
    started = false,
  }

  local function json_response(status, value)
    local raw = nil
    if JSON and JSON.encode then
      local ok, encoded = pcall(function() return JSON.encode(value) end)
      if ok and type(encoded) == "string" then raw = encoded end
    end
    if not raw then
      status = "500 Internal Server Error"
      raw = "{\"ok\":false,\"error\":\"json encode failed\"}"
    end
    return {
      status = status or "200 OK",
      type = "application/json; charset=utf-8",
      headers = {
        ["cache-control"] = "no-store",
        ["connection"] = "close",
      },
      body = raw,
    }
  end

  local function html_response(body)
    return {
      status = "200 OK",
      type = "text/html; charset=utf-8",
      headers = {
        ["cache-control"] = "no-store",
        ["connection"] = "close",
      },
      body = body,
    }
  end

  local function redirect_response(target)
    return {
      status = "302 Found",
      type = "text/plain; charset=utf-8",
      headers = {
        ["location"] = target,
        ["cache-control"] = "no-store",
        ["connection"] = "close",
      },
      body = "Redirecting",
    }
  end

  local function decode_body(req)
    if not req or not req.getbody or not JSON or not JSON.decode then return nil end
    local ok_body, raw = pcall(function() return req.getbody() end)
    if not ok_body or type(raw) ~= "string" or raw == "" then return nil end
    local ok_json, doc = pcall(function() return JSON.decode(raw) end)
    if ok_json and type(doc) == "table" then return doc end
    return nil
  end

  local function bool_value(value, fallback)
    if type(value) == "boolean" then return value end
    if type(value) == "number" then return value ~= 0 end
    local text = tostring(value or ""):lower()
    if text == "true" or text == "1" or text == "on" or text == "yes" then return true end
    if text == "false" or text == "0" or text == "off" or text == "no" then return false end
    return fallback
  end

  local function int_value(value, fallback, min_value, max_value)
    local number = math.floor(tonumber(value) or tonumber(fallback) or 0)
    if min_value and number < min_value then number = min_value end
    if max_value and number > max_value then number = max_value end
    return number
  end

  local function clean_path(value, fallback)
    local text = text_or(value, fallback or "")
    text = text:gsub("%s+", "")
    if text == "" then text = fallback or "" end
    if text:sub(1, 1) ~= "/" then text = "/" .. text end
    return text
  end

  local function clean_host(value)
    local text = text_or(value, ""):gsub("^%s+", ""):gsub("%s+$", "")
    text = text:gsub("[^%w%.%-_:]", "")
    if text == "" then text = text_or(config.host, "192.168.3.3") end
    return text
  end

  local function public_config()
    return {
      ok = true,
      host = text_or(config.host, "192.168.3.3"),
      port = int_value(config.port, 17865, 1, 65535),
      poll_ms = int_value(config.poll_ms, 1000, 250, 10000),
      timeout_ms = int_value(config.timeout_ms, 2500, 500, 30000),
      status_path = clean_path(config.status_path, "/status"),
      control_path = clean_path(config.control_path, "/control"),
      serial_log = bool_value(config.serial_log, true),
      bridge_url = "http://" .. text_or(config.host, "192.168.3.3") .. ":" .. tostring(int_value(config.port, 17865, 1, 65535)),
      route_base = self.route_base,
    }
  end

  local function write_config()
    local body = table.concat({
      "local config = {}",
      "",
      string.format("config.host = %q", text_or(config.host, "192.168.3.3")),
      string.format("config.port = %d", int_value(config.port, 17865, 1, 65535)),
      string.format("config.poll_ms = %d", int_value(config.poll_ms, 1000, 250, 10000)),
      string.format("config.control_path = %q", clean_path(config.control_path, "/control")),
      string.format("config.status_path = %q", clean_path(config.status_path, "/status")),
      string.format("config.timeout_ms = %d", int_value(config.timeout_ms, 2500, 500, 30000)),
      string.format("config.serial_log = %s", bool_value(config.serial_log, true) and "true" or "false"),
      "",
      "return config",
      "",
    }, "\n")
    if file and file.putcontents then
      return pcall(function() return file.putcontents(APP_DIR .. "/config.lua", body) end)
    end
    if file and file.open then
      local fd = file.open(APP_DIR .. "/config.lua", "w+")
      if fd then
        local ok = pcall(function() fd:write(body) end)
        pcall(function() fd:close() end)
        return ok
      end
    end
    return false
  end

  local function route_config()
    return json_response("200 OK", public_config())
  end

  local function route_save(req)
    local doc = decode_body(req)
    if type(doc) ~= "table" then
      return json_response("400 Bad Request", { ok = false, error = "invalid json body" })
    end

    config.host = clean_host(doc.host)
    config.port = int_value(doc.port, config.port or 17865, 1, 65535)
    config.poll_ms = int_value(doc.poll_ms, config.poll_ms or 1000, 250, 10000)
    config.timeout_ms = int_value(doc.timeout_ms, config.timeout_ms or 2500, 500, 30000)
    config.status_path = clean_path(doc.status_path, config.status_path or "/status")
    config.control_path = clean_path(doc.control_path, config.control_path or "/control")
    config.serial_log = bool_value(doc.serial_log, config.serial_log ~= false)

    local ok = write_config()
    if not ok then
      return json_response("500 Internal Server Error", { ok = false, error = "failed to write config.lua" })
    end

    if APP then
      APP.last_seen = 0
      APP.data = nil
      APP.lyric_key = ""
      APP.lyrics = {}
      APP.lyric_idx = 1
      if APP.poll_status then pcall(APP.poll_status) end
    end

    return json_response("200 OK", public_config())
  end

  local INDEX_HTML = [==[
<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>SMTC Music 设置</title>
<style>
:root{color-scheme:light;--bg:#f6f8fb;--surface:#fff;--text:#111827;--muted:#64748b;--line:#dbe3ef;--blue:#2563eb;--green:#16a34a;--danger:#b91c1c}
*{box-sizing:border-box}body{margin:0;background:linear-gradient(180deg,#fbfdff,var(--bg));color:var(--text);font:15px/1.55 system-ui,-apple-system,"Segoe UI","Microsoft YaHei",sans-serif}
main{width:min(860px,calc(100% - 24px));margin:22px auto;display:grid;gap:14px}.hero,.card{background:var(--surface);border:1px solid var(--line);border-radius:10px;box-shadow:0 14px 34px rgba(15,23,42,.08)}
.hero{display:grid;grid-template-columns:72px minmax(0,1fr);gap:16px;align-items:center;padding:18px}.icon{width:72px;height:72px;border-radius:16px;object-fit:cover;background:#111827}.kicker{margin:0 0 4px;color:var(--blue);font-weight:800;font-size:13px}
h1{margin:0;font-size:32px;line-height:1.1}.summary{margin:8px 0 0;color:var(--muted)}.card{padding:18px}h2{margin:0 0 14px;font-size:20px}.grid{display:grid;grid-template-columns:1fr 1fr;gap:12px}
label{display:grid;gap:7px;color:#334155;font-weight:700}input{width:100%;min-height:44px;border:1px solid var(--line);border-radius:8px;padding:0 12px;background:#fff}.check{display:flex;gap:10px;align-items:center}.check input{width:auto;min-height:auto}
.actions{display:flex;flex-wrap:wrap;gap:10px;margin-top:16px}button{min-height:42px;border:1px solid var(--line);border-radius:8px;padding:0 14px;background:#fff;font-weight:800;cursor:pointer}.primary{border-color:#bfdbfe;background:#eff6ff;color:var(--blue)}
.status{min-height:24px;color:var(--muted)}.status.ok{color:var(--green)}.status.err{color:var(--danger)}code{background:#f1f5f9;border-radius:6px;padding:2px 5px}.hint{margin:12px 0 0;color:var(--muted);font-size:13px}
@media(max-width:640px){.hero{grid-template-columns:56px minmax(0,1fr);padding:14px}.icon{width:56px;height:56px;border-radius:13px}h1{font-size:27px}.grid{grid-template-columns:1fr}}
</style>
</head>
<body>
<main>
  <section class="hero">
    <img class="icon" src="__APP_ICON_URL__" alt="SMTC Music 图标">
    <div>
      <p class="kicker">HoloCubic App WebUI</p>
      <h1>SMTC Music 设置</h1>
      <p class="summary">配置设备端连接的 PC bridge 地址、轮询间隔和接口路径。保存后会写入 <code>config.lua</code> 并立即重连。</p>
    </div>
  </section>
  <section class="card">
    <h2>Bridge 连接</h2>
    <form id="form">
      <div class="grid">
        <label>电脑 IP / 主机名<input id="host" name="host" autocomplete="off" placeholder="192.168.3.3"></label>
        <label>端口<input id="port" name="port" type="number" min="1" max="65535" step="1"></label>
        <label>状态接口<input id="status_path" name="status_path" placeholder="/status"></label>
        <label>控制接口<input id="control_path" name="control_path" placeholder="/control"></label>
        <label>轮询间隔 ms<input id="poll_ms" name="poll_ms" type="number" min="250" max="10000" step="50"></label>
        <label>请求超时 ms<input id="timeout_ms" name="timeout_ms" type="number" min="500" max="30000" step="100"></label>
      </div>
      <p class="check"><input id="serial_log" name="serial_log" type="checkbox"><span>启用串口日志</span></p>
      <div class="actions">
        <button class="primary" id="save" type="submit">保存设置</button>
        <button id="reload" type="button">重新读取</button>
        <a id="bridge" href="#" target="_blank" rel="noopener"><button type="button">打开 Bridge</button></a>
      </div>
      <p class="hint">当前 Bridge：<code id="bridgeUrl">-</code></p>
      <p id="status" class="status">正在读取配置...</p>
    </form>
  </section>
</main>
<script>
const $=id=>document.getElementById(id);
function setStatus(text,tone){const n=$("status");n.textContent=text;n.className="status "+(tone||"")}
function payload(){return{host:$("host").value.trim(),port:Number($("port").value),status_path:$("status_path").value.trim(),control_path:$("control_path").value.trim(),poll_ms:Number($("poll_ms").value),timeout_ms:Number($("timeout_ms").value),serial_log:$("serial_log").checked}}
function fill(data){$("host").value=data.host||"";$("port").value=data.port||17865;$("status_path").value=data.status_path||"/status";$("control_path").value=data.control_path||"/control";$("poll_ms").value=data.poll_ms||1000;$("timeout_ms").value=data.timeout_ms||2500;$("serial_log").checked=!!data.serial_log;$("bridgeUrl").textContent=data.bridge_url||"-";$("bridge").href=data.bridge_url||"#"}
async function load(){setStatus("正在读取配置...","");const data=await fetch("api/config",{cache:"no-store"}).then(r=>r.json());if(!data.ok)throw new Error(data.error||"读取失败");fill(data);setStatus("配置已加载","ok")}
async function save(ev){ev.preventDefault();$("save").disabled=true;setStatus("正在保存...","");try{const res=await fetch("api/config",{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(payload())});const data=await res.json();if(!data.ok)throw new Error(data.error||"保存失败");fill(data);setStatus("已保存，设备端正在使用新配置","ok")}catch(err){setStatus(err.message||String(err),"err")}finally{$("save").disabled=false}}
$("form").addEventListener("submit",save);$("reload").addEventListener("click",()=>load().catch(err=>setStatus(err.message||String(err),"err")));load().catch(err=>setStatus(err.message||String(err),"err"));
</script>
</body>
</html>
]==]
  INDEX_HTML = INDEX_HTML:gsub("__APP_ICON_URL__", app_asset_url("main.png"), 1)

  local function route_index()
    return html_response(INDEX_HTML)
  end

  local function route_redirect()
    return redirect_response(self.route_base .. "/")
  end

  function self:register(method, route, handler)
    if not httpd or not httpd.dynamic then return false end
    local ok, err = pcall(function() return httpd.dynamic(method, route, handler) end)
    if not ok or err then
      print("[smtc_music] web route skipped", tostring(route), tostring(err))
      return false
    end
    self.routes[#self.routes + 1] = { method = method, route = route }
    return true
  end

  function self:start()
    if self.started or not httpd or not httpd.start or not httpd.dynamic then return false end
    pcall(function()
      httpd.start({ webroot = "/sd", auto_index = httpd.INDEX_NONE, max_handlers = 36 })
    end)
    local base = app and app.route_base and app.route_base() or ""
    if base == "" then base = "/holocubic-smtc-music" end
    self.route_base = base
    self:register(httpd.GET, base, route_redirect)
    self:register(httpd.GET, base .. "/", route_index)
    self:register(httpd.GET, base .. "/api/config", route_config)
    self:register(httpd.POST, base .. "/api/config", route_save)
    if app and app.set_webui then pcall(function() app.set_webui(true) end) end
    self.started = true
    return true
  end

  function self:stop()
    if not httpd or not httpd.unregister then
      self.routes = {}
      return
    end
    for i = #self.routes, 1, -1 do
      local item = self.routes[i]
      pcall(function() httpd.unregister(item.method, item.route) end)
    end
    self.routes = {}
    if app and app.set_webui then pcall(function() app.set_webui(false) end) end
    self.started = false
  end

  return self
end

return M
