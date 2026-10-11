param(
  [string]$Model = "deepseek-v4-flash",
  [string]$Effort = "high"
)
# 一个真正需要多步推理、且答案不显然的问题
$q = "A bat and a ball cost 1.10 dollars in total. The bat costs 1.00 dollar more than the ball. How much does the ball cost? Give the final number only."
$obj = [ordered]@{
  model = $Model
  messages = @(@{role="user"; content=$q})
  max_tokens = 1024
  stream = $true
}
if ($Effort -ne "") { $obj["reasoning_effort"] = $Effort }
$body = $obj | ConvertTo-Json -Depth 6 -Compress
$tag = if ($Effort -eq "") { "default(无参数)" } else { $Effort }

$resp = Invoke-WebRequest "http://127.0.0.1:8799/workbuddy/v1/chat/completions" -Method POST -Body $body -ContentType "application/json" -UseBasicParsing -TimeoutSec 120
$reason = ""; $content = ""
foreach ($line in ($resp.Content -split "`n")) {
  $t = $line.Trim()
  if (-not $t.StartsWith("data:")) { continue }
  $d = $t.Substring(5).Trim()
  if ($d -eq "[DONE]" -or $d -eq "") { continue }
  try { $j = $d | ConvertFrom-Json } catch { continue }
  $delta = $j.choices[0].delta
  if ($delta.reasoning_content) { $reason += "$($delta.reasoning_content)" }
  if ($delta.content) { $content += "$($delta.content)" }
}
Write-Output "########## model=$Model effort=[$tag] ##########"
Write-Output "----- reasoning_content -----"
Write-Output $reason
Write-Output "----- content (final answer) -----"
Write-Output $content
Write-Output ""
