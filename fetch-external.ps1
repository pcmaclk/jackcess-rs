# Downloads the reference material the verification harnesses need:
#   external/jackcess-encrypt-5.0.2.jar      (crypto reference)
#   external/bcprov-jdk18on-1.86.jar         (jackcess-encrypt dependency)
#   external/jackcess-test-resources/        (real Access databases, V1997..V2019)
#   external/jackcessencrypt-test/           (real encrypted/encoded/Money files)
# It also fetches the jackcess-5.0.4 jar/sources at the repository root when
# they are missing.  Pass -Proxy http://127.0.0.1:10808 when GitHub/Maven need
# a local proxy.
param(
  [string]$Proxy = "",
  [switch]$Force
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Set-Location $root
$external = Join-Path $root "external"
New-Item -ItemType Directory -Force -Path $external | Out-Null

function Get-File([string]$uri, [string]$out) {
  if ((Test-Path $out) -and -not $Force) {
    Write-Host "exists  $out"
    return
  }
  Write-Host "fetch   $uri"
  $args = @{ Uri = $uri; OutFile = $out; TimeoutSec = 300; UseBasicParsing = $true }
  if ($Proxy) { $args.Proxy = $Proxy }
  Invoke-WebRequest @args
}

$maven = "https://repo1.maven.org/maven2"
Get-File "$maven/com/healthmarketscience/jackcess/jackcess/5.0.4/jackcess-5.0.4.jar" (Join-Path $root "jackcess-5.0.4.jar")
Get-File "$maven/com/healthmarketscience/jackcess/jackcess/5.0.4/jackcess-5.0.4-sources.jar" (Join-Path $root "jackcess-5.0.4-sources.jar")
Get-File "$maven/com/healthmarketscience/jackcess/jackcess-encrypt/5.0.2/jackcess-encrypt-5.0.2.jar" (Join-Path $external "jackcess-encrypt-5.0.2.jar")
Get-File "$maven/org/bouncycastle/bcprov-jdk18on/1.86/bcprov-jdk18on-1.86.jar" (Join-Path $external "bcprov-jdk18on-1.86.jar")

function Expand-TestData([string]$uri, [string]$archiveName, [string]$prefix, [string]$sourcePath, [string]$destination) {
  if ((Test-Path $destination) -and -not $Force) {
    Write-Host "exists  $destination"
    return
  }
  $tar = Join-Path $external $archiveName
  Get-File $uri $tar
  $tmp = Join-Path $external ("_extract_" + [guid]::NewGuid().ToString("N"))
  New-Item -ItemType Directory -Force -Path $tmp | Out-Null
  try {
    & tar xzf $tar -C $tmp
    if ($LASTEXITCODE -ne 0) { throw "tar failed for $tar" }
    $src = Join-Path $tmp (Join-Path $prefix $sourcePath)
    if (-not (Test-Path $src)) { throw "archive did not contain $prefix/$sourcePath" }
    Move-Item $src $destination
    Write-Host "extract $destination"
  } finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item $tar -Force -ErrorAction SilentlyContinue
  }
}

# extract the reference sources (optional; used for reading the Java code)
$srcDir = Join-Path $root "jackcess-src"
if (-not (Test-Path $srcDir)) {
  New-Item -ItemType Directory -Force -Path $srcDir | Out-Null
  & tar xf (Join-Path $root "jackcess-5.0.4-sources.jar") -C $srcDir
  if ($LASTEXITCODE -ne 0) { throw "extracting jackcess-5.0.4-sources.jar failed" }
  Write-Host "extract $srcDir"
}

Expand-TestData "https://codeload.github.com/spannm/jackcess/tar.gz/refs/heads/master" "jackcess-master.tar.gz" "jackcess-master" "src/test/resources" (Join-Path $external "jackcess-test-resources")
Expand-TestData "https://codeload.github.com/jahlborn/jackcessencrypt/tar.gz/refs/heads/master" "jackcessencrypt-master.tar.gz" "jackcessencrypt-master" "src/test/data" (Join-Path $external "jackcessencrypt-test")

Write-Host "reference material ready under $external"


