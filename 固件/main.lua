local previous = rawget(_G, "SMTC_MUSIC_APP")
if previous and previous.stop then
  pcall(function() previous.stop("reload") end)
end

local FALLBACK_APP_ID = "holocubic-smtc-music"

local function resolve_app_dir()
  local current = app and app.current and app.current() or nil
  local entry = current and current.entry or nil
  if type(entry) == "string" and entry ~= "" then
    entry = entry:gsub("\\", "/")
    local dir = entry:match("^(.*)/[^/]*$")
    if dir and dir ~= "" then return dir end
  end

  local fallback = "/sd/apps/" .. FALLBACK_APP_ID
  if file and file.exists and not file.exists(fallback .. "/config.lua") then
    local candidates = { "package", FALLBACK_APP_ID .. "/package", FALLBACK_APP_ID }
    for _, dir in ipairs(candidates) do
      if file.exists(dir .. "/config.lua") then return dir end
    end
  end
  return fallback
end

local APP_DIR = resolve_app_dir()

local config = dofile(APP_DIR .. "/config.lua")
local ui_module = dofile(APP_DIR .. "/ui.lua")
local web_module = dofile(APP_DIR .. "/web.lua")
local JSON = rawget(_G, "sjson") or rawget(_G, "json")

local APP = {
  timer = nil,
  control_timer = nil,
  ui_timer = nil,
  running = true,
  inflight = false,
  cover_inflight = false,
  cover_key = "",
  lyric_inflight = false,
  lyric_key = "",
  lyrics = {},
  lyric_idx = 1,
  last_seen = 0,
  sync_position_ms = 0,
  sync_duration_ms = 0,
  sync_at_ms = 0,
  playback_state = "unknown",
  data = nil,
  ui = {},
  font = nil,
  view = nil,
  web = nil,
}
_G.SMTC_MUSIC_APP = APP

local function log(...)
  if config.serial_log ~= false then print("[smtc_music]", ...) end
end

local function now_ms()
  if millis then
    local ok, value = pcall(millis)
    if ok and type(value) == "number" then return value end
  end
  if tmr and tmr.now then
    local ok, value = pcall(function() return tmr.now() end)
    if ok and type(value) == "number" then return math.floor(value / 1000) end
  end
  return 0
end

local function text_or(value, fallback)
  if value == nil then return fallback or "" end
  local text = tostring(value)
  if text == "" then return fallback or "" end
  return text
end

local function decode(raw)
  if not JSON or not JSON.decode or type(raw) ~= "string" then return nil end
  local ok, value = pcall(JSON.decode, raw)
  return ok and value or nil
end

local function base_url()
  return "http://" .. tostring(config.host) .. ":" .. tostring(config.port)
end

local function media_key(provider, id)
  provider = text_or(provider, "")
  id = text_or(id, "")
  if provider == "" or id == "" then return "" end
  return provider .. "_" .. id
end

local function status_lyric_ref(data)
  if not data then return "", "", "" end
  local provider = text_or(data.lyric_provider, "")
  local id = text_or(data.lyric_id_text, "")
  if id == "" then
    id = text_or(data.ncm_id_text, "")
    if id == "" then
      local raw = tonumber(data.ncm_id) or 0
      if raw > 0 then id = tostring(math.floor(raw)) end
    end
    if id ~= "" and provider == "" then provider = "netease" end
  end
  return provider, id, media_key(provider, id)
end

APP.view = ui_module.new({
  app = APP,
  app_dir = APP_DIR,
  config = config,
  now_ms = now_ms,
  text_or = text_or,
  base_url = base_url,
})

local function render(sync_media)
  if APP.view and APP.view.render then APP.view:render(sync_media) end
end

local function fetch_lyrics(provider, id)
  provider = text_or(provider, "")
  id = text_or(id, "")
  local key = media_key(provider, id)
  if key == "" or APP.lyric_inflight or not http or not http.get then return end
  if APP.lyric_key == key and #(APP.lyrics or {}) > 0 then return end
  APP.lyric_inflight = true
  local url = base_url() .. "/lyrics?provider=" .. tostring(provider) .. "&id=" .. tostring(id)
  http.get(url, { timeout = tonumber(config.timeout_ms) or 5000 }, function(code, body)
    APP.lyric_inflight = false
    if not APP.running then return end
    if code == 200 then
      local data = decode(body)
      if type(data) == "table" and type(data.lines) == "table" then
        APP.lyric_key = key
        APP.lyrics = data.lines
        APP.lyric_idx = 1
        render(false)
        return
      end
    end
    APP.lyric_key = key
    APP.lyrics = {}
    APP.lyric_idx = 1
    render(false)
  end)
end

local function poll_status()
  if APP.inflight or not http or not http.get then return end
  APP.inflight = true
  local url = base_url() .. tostring(config.status_path or "/status")
  http.get(url, { timeout = tonumber(config.timeout_ms) or 2500 }, function(code, body)
    APP.inflight = false
    if not APP.running then return end
    if code == 200 then
      local data = decode(body)
      if type(data) == "table" then
        APP.data = data
        APP.last_seen = now_ms()
        APP.sync_at_ms = APP.last_seen
        APP.sync_position_ms = tonumber(data.position_ms) or 0
        APP.sync_duration_ms = tonumber(data.duration_ms) or 0
        APP.playback_state = text_or(data.state, "unknown")
        local provider, id, key = status_lyric_ref(data)
        if key ~= "" and key ~= APP.lyric_key then
          APP.lyrics = {}
          APP.lyric_idx = 1
          fetch_lyrics(provider, id)
        elseif key == "" and APP.lyric_key ~= "" then
          APP.lyric_key = ""
          APP.lyrics = {}
          APP.lyric_idx = 1
        end
        render(true)
        return
      end
    end
    APP.data = { ok = false, connected = false, state = "offline" }
    APP.playback_state = "offline"
    render(true)
  end)
end

APP.poll_status = poll_status

local function control(action)
  if not http or not http.get then return end
  local url = base_url() .. tostring(config.control_path or "/control") .. "?action=" .. tostring(action)
  http.get(url, { timeout = tonumber(config.timeout_ms) or 2500 }, function(code, body)
    log("control", action, code)
  end)
  if APP.control_timer then
    pcall(function() APP.control_timer:stop() end)
    pcall(function() APP.control_timer:unregister() end)
    APP.control_timer = nil
  end
  if tmr and tmr.create then
    APP.control_timer = tmr.create()
    APP.control_timer:alarm(420, tmr.ALARM_SINGLE or 0, function()
      APP.control_timer = nil
      poll_status()
    end)
  else
    poll_status()
  end
end

local function bind_keys()
  if not key or not key.on then return end
  if app and app.set_home_exit then pcall(function() app.set_home_exit(false) end) end
  key.on(key.LEFT, function(evt) if evt == key.SHORT then control("previous") end end)
  key.on(key.RIGHT, function(evt) if evt == key.SHORT then control("next") end end)
  key.on(key.HOME, function(evt)
    if evt == key.SHORT then control("playpause")
    elseif evt == key.LONG_START or evt == key.EXIT then
      APP.stop("exit")
      if app and app.exit then app.exit() end
    end
  end)
  if key.UP then key.on(key.UP, function(evt) if evt == key.SHORT then control("seek_forward") end end) end
  if key.DOWN then key.on(key.DOWN, function(evt) if evt == key.SHORT then control("seek_back") end end) end
end

function APP.stop(reason)
  if not APP.running then return end
  APP.running = false
  if APP.timer then pcall(function() APP.timer:stop() end); pcall(function() APP.timer:unregister() end); APP.timer = nil end
  if APP.ui_timer then pcall(function() APP.ui_timer:stop() end); pcall(function() APP.ui_timer:unregister() end); APP.ui_timer = nil end
  if APP.control_timer then pcall(function() APP.control_timer:stop() end); pcall(function() APP.control_timer:unregister() end); APP.control_timer = nil end
  if key and key.off then pcall(key.off) end
  if app and app.set_home_exit then pcall(function() app.set_home_exit(true) end) end
  if APP.view and APP.view.stop then APP.view:stop() end
  if APP.web and APP.web.stop then APP.web:stop() end
end

APP.view:build()
APP.web = web_module.new({
  app = APP,
  app_dir = APP_DIR,
  config = config,
  json = JSON,
  text_or = text_or,
})
APP.web:start()
bind_keys()
poll_status()
if tmr and tmr.create then
  APP.timer = tmr.create()
  APP.timer:alarm(tonumber(config.poll_ms) or 1000, tmr.ALARM_AUTO, poll_status)
  APP.ui_timer = tmr.create()
  APP.ui_timer:alarm(120, tmr.ALARM_AUTO, function()
    if APP.running then render(false) end
  end)
end
