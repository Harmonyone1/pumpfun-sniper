$apiKey = "066a76e6-916f-4ef2-9194-c86676072933"
$body = @{
    jsonrpc = "2.0"
    id = 1
    method = "getTokenAccountsByOwner"
    params = @(
        "C9ibhqLMz68HewsMXiZyXVAiJ68uLg53vSsSuyLQWYA6",
        @{ programId = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb" },
        @{ encoding = "jsonParsed" }
    )
} | ConvertTo-Json -Depth 10

$response = Invoke-RestMethod -Uri "https://mainnet.helius-rpc.com/?api-key=$apiKey" -Method POST -Body $body -ContentType 'application/json'

if ($response.result.value.Count -eq 0) {
    Write-Output "No Token-2022 tokens found in wallet - sale was successful!"
} else {
    foreach ($acct in $response.result.value) {
        $info = $acct.account.data.parsed.info
        Write-Output "Mint: $($info.mint)"
        Write-Output "Balance: $($info.tokenAmount.uiAmount)"
    }
}
