# Disposable PostgreSQL TLS fixture. Outputs container id, assigned port, CA path.
param([Parameter(Mandatory = $true)][string]$Fixture)
$ErrorActionPreference = 'Stop'
$fixturePath = [IO.Path]::GetFullPath($Fixture)
New-Item -ItemType Directory -Force -Path $fixturePath | Out-Null
$openssl = 'C:\Program Files\Git\usr\bin\openssl.exe'
if (-not (Test-Path -LiteralPath $openssl)) { $openssl = (Get-Command openssl).Source }
$ErrorActionPreference = 'Continue'
& $openssl req -x509 -nodes -newkey rsa:2048 -days 2 -subj '/CN=Good Query Test CA' -addext 'basicConstraints=critical,CA:TRUE' -keyout "$fixturePath/ca.key" -out "$fixturePath/ca.pem" 2>$null
if ($LASTEXITCODE -ne 0) { throw 'CA generation failed' }
& $openssl req -nodes -newkey rsa:2048 -subj '/CN=localhost' -keyout "$fixturePath/server.key" -out "$fixturePath/server.csr" 2>$null
if ($LASTEXITCODE -ne 0) { throw 'Server key generation failed' }
@('subjectAltName=DNS:localhost', 'extendedKeyUsage=serverAuth', 'keyUsage=digitalSignature,keyEncipherment', 'basicConstraints=CA:FALSE') | Set-Content -LiteralPath "$fixturePath/server.ext" -Encoding ascii
& $openssl x509 -req -in "$fixturePath/server.csr" -CA "$fixturePath/ca.pem" -CAkey "$fixturePath/ca.key" -CAcreateserial -days 2 -extfile "$fixturePath/server.ext" -out "$fixturePath/server.pem" 2>$null
if ($LASTEXITCODE -ne 0) { throw 'Server certificate generation failed' }
$ErrorActionPreference = 'Stop'
$container = docker run --rm -e POSTGRES_PASSWORD=postgres -p '127.0.0.1::5432' -v "${fixturePath}:/tls:ro" -d postgres:17 bash -c 'cp /tls/server.pem /tmp/server.pem && cp /tls/server.key /tmp/server.key && chmod 600 /tmp/server.key && chown postgres:postgres /tmp/server.key /tmp/server.pem && exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key'
if ($LASTEXITCODE -ne 0) { throw 'PostgreSQL TLS fixture did not start' }
try {
    $ready = $false
    for ($attempt = 0; $attempt -lt 100; $attempt++) {
        docker exec $container pg_isready -h 127.0.0.1 -U postgres *> $null
        if ($LASTEXITCODE -eq 0) { $ready = $true; break }
        Start-Sleep -Milliseconds 200
    }
    if (-not $ready) { throw 'PostgreSQL TLS fixture did not become ready' }
    $binding = docker port $container 5432/tcp
    if ($LASTEXITCODE -ne 0) { throw 'Could not read fixture port' }
    Write-Output $container
    Write-Output ($binding.Split(':')[-1])
    Write-Output "$fixturePath/ca.pem"
} catch {
    docker stop $container *> $null
    throw
}
