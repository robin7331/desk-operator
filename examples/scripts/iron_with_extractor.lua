-- Power path for a soldering iron that should never run without its fume extractor.
-- Two outlets, switched in order, both configured through params:
--
--   power = { script = "scripts/iron_with_extractor.lua", params = {
--     iron      = { provider = "sim", endpoint = "station_iron" },
--     extractor = { provider = "sim", endpoint = "station_fan" },
--   } }
--
-- Give a scripted device its own outlets: if another device switches one of them, the two
-- disagree about what is on.

local function outlets(ctx)
  return ctx.power(ctx.params.iron), ctx.power(ctx.params.extractor)
end

function on(ctx)
  local iron, extractor = outlets(ctx)
  extractor:on()
  ctx.sleep(300) -- let the fan spin up first
  iron:on()
  ctx.log("extractor and iron on")
end

function off(ctx)
  local iron, extractor = outlets(ctx)
  iron:off()
  ctx.sleep(ctx.params.extractor_run_on or 500) -- clear the remaining fumes
  extractor:off()
  ctx.log("iron and extractor off")
end

-- "on" only when both are on; anything half-switched is honestly "unknown".
function state(ctx)
  local iron, extractor = outlets(ctx)
  local a, b = iron:state(), extractor:state()
  if a == b then
    return a
  end
  return "unknown"
end
