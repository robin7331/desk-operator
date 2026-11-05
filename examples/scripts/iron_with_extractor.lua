-- Power path for a soldering iron that should never run without its fume extractor.
-- Two outlets, switched in order, both configured through params:
--
--   power = { script = "scripts/iron_with_extractor.lua", params = {
--     iron      = { provider = "sim", endpoint = "outlet4" },
--     extractor = { provider = "sim", endpoint = "outlet5" },
--   } }

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

function state(ctx)
  local iron = outlets(ctx)
  return iron:state()
end
