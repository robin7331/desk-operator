-- A capture driver for stock sigrok's `demo` device: maps the action's arguments to
-- ctx.sigrok.acquire and describes the file it produced.

PARAMS = {}

-- sigrok's output module -> the file's extension.
local EXTENSIONS = { srzip = "sr", binary = "bin", csv = "csv", vcd = "vcd" }

function action(ctx, name, args)
  if name ~= "capture" then
    error("unknown action " .. name)
  end
  local ms = math.ceil(args.duration * 1000)
  local r = ctx.sigrok.acquire{
    config       = { samplerate = args.samplerate },
    channels     = args.channels,
    time         = ms,
    triggers     = args.trigger,
    wait_trigger = args.trigger ~= nil,
    format       = args.format,
    into         = ctx.output,
    -- The capture, plus time to start sigrok and to wait for a trigger.
    timeout      = ms + (args.trigger and 30000 or 10000),
  }
  return {
    format     = args.format,
    extension  = EXTENSIONS[args.format],
    samplerate = args.samplerate,
    channels   = args.channels,
    duration   = args.duration,
    trigger    = args.trigger,
    bytes      = r.bytes,
  }
end
