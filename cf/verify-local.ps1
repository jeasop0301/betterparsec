# Local end-to-end check for the distribution worker (wrangler dev on :8788).
$landing = curl.exe -s -o NUL -w '%{http_code}' http://127.0.0.1:8788/
$wrong = curl.exe -s -o NUL -w '%{http_code}' -X POST -d 'password=wrong' http://127.0.0.1:8788/api/download
$right = curl.exe -s -o NUL -w '%{http_code} %{size_download} %{content_type}' -X POST -d 'password=local-test-password' http://127.0.0.1:8788/api/download
Write-Output "landing=$landing"
Write-Output "wrongpw=$wrong"
Write-Output "rightpw=$right"
