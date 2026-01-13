$apiKey = "066a76e6-916f-4ef2-9194-c86676072933"
$txSig = "3uNp5PB9dPrS7Ppoocfh6sBQcEGx2z7yBWZshnnQM4YRiRTTdp5xYjVwtY5AgYR8RJcgkfmt8nxg4ALKTN3avd5r"

$body = @{
    jsonrpc = "2.0"
    id = 1
    method = "getTransaction"
    params = @(
        $txSig,
        @{ encoding = "jsonParsed"; maxSupportedTransactionVersion = 0 }
    )
} | ConvertTo-Json -Depth 10

$response = Invoke-RestMethod -Uri "https://mainnet.helius-rpc.com/?api-key=$apiKey" -Method POST -Body $body -ContentType 'application/json'

if ($null -eq $response.result) {
    Write-Output "Transaction not found or failed"
    Write-Output $response | ConvertTo-Json -Depth 5
} else {
    Write-Output "Transaction status:"
    if ($response.result.meta.err) {
        Write-Output "ERROR: $($response.result.meta.err | ConvertTo-Json)"
    } else {
        Write-Output "SUCCESS - no errors"
    }
    Write-Output "Slot: $($response.result.slot)"
}
