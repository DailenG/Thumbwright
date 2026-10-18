<#
.SYNOPSIS
    Downloads the real camera RAW samples used by the ignored `real_raw_samples` test.

.DESCRIPTION
    Every sample is a CC0 (public domain) upload to https://raw.pixls.us/ and is pinned by
    SHA-256 (the hashes below are the ones the site publishes in its repository index).
    Files that already exist with the right hash are not downloaded again; a download
    whose hash differs is deleted and reported as an error.

    Run the test afterwards:
        $env:TW_SAMPLES = (Resolve-Path target\samples).Path
        cargo test -p tw-core --test samples -- --ignored

.PARAMETER Dest
    Target directory. Defaults to <repo>\target\samples (git-ignored).
#>
[CmdletBinding()]
param(
    [string]$Dest = (Join-Path (Split-Path -Parent $PSScriptRoot) 'target\samples')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$samples = @(
    @{ Name = 'canon-eos-r6.cr3'; Sha256 = '74abb0a113d075ad9887a058082f40dd2a938c4813a08474d82356f11a027778'; Url = 'https://raw.pixls.us/getfile.php/4659/nice/Canon%20-%20EOS%20R6%20-%203%3A2.CR3' },
    @{ Name = 'canon-eos-r6-mark-iii.cr3'; Sha256 = '7e1a5d84297a52853b0bbe5a5c03a5d4f5b93e00b7020271523240f9cabfe50e'; Url = 'https://raw.pixls.us/getfile.php/8964/nice/Canon%20-%20EOS%20R6%20Mark%20III%20-%20CRAW%20%283%3A2%29.CR3' },
    @{ Name = 'canon-eos-m50.cr3'; Sha256 = '27a88fdae82516ba136e12c2cc9b934f25dbaaac24d64ab4e5517f8524463594'; Url = 'https://raw.pixls.us/getfile.php/4656/nice/Canon%20-%20EOS%20M50%20-%203%3A2.CR3' },
    @{ Name = 'canon-eos-r10-rotated.cr3'; Sha256 = '15d017f0770b3da17180077617ffe07b786a1ec2cb36297f81a82a27a54756db'; Url = 'https://raw.pixls.us/getfile.php/5907/nice/Canon%20-%20EOS%20R10%20-%203%3A2.CR3' },
    @{ Name = 'canon-eos-d30.crw'; Sha256 = '4ee482f2b6f9f1a8d370703ea04e9f2abaa49356973a7f56b229af93a78e8a2e'; Url = 'https://raw.pixls.us/getfile.php/1307/nice/Canon%20-%20EOS%20D30%20-%20RAW%20%283%3A2%29.CRW' },
    @{ Name = 'canon-eos-300d-rotated.crw'; Sha256 = '47da16e1668af5500eea63cf42cf2992d9cb3d32180b8d786f17f1e2a57f39f4'; Url = 'https://raw.pixls.us/getfile.php/1983/nice/Canon%20-%20EOS%20300D%20-%20RAW%20%283%3A2%29.crw' },
    @{ Name = 'canon-powershot-g5.crw'; Sha256 = 'bb084223d714871b99de4f549eaf00513e30a7080feeead700252c2008060196'; Url = 'https://raw.pixls.us/getfile.php/1746/nice/Canon%20-%20PowerShot%20G5%20-%20RAW%20%284%3A3%29.CRW' },
    @{ Name = 'sigma-dp1.x3f'; Sha256 = '44508f7df4191aacaecb6461e1841dae002e5b71a4f95a00aa3f52d1c6d6bc5f'; Url = 'https://raw.pixls.us/getfile.php/1116/nice/Sigma%20-%20DP1%20-%203%3A2.X3F' },
    @{ Name = 'sigma-sd15.x3f'; Sha256 = 'e9bf8ca29bb9c533b764a4af7830bcf029ee6792323bad3e4179d0e5c29389c0'; Url = 'https://raw.pixls.us/getfile.php/1863/nice/Sigma%20-%20SIGMA%20SD15%20-%203%3A2.X3F' },
    @{ Name = 'sigma-sd-quattro.x3f'; Sha256 = 'bf26469746e5937ca4f9cfb34a022891f9c8518febd676355930b805b9307b48'; Url = 'https://raw.pixls.us/getfile.php/6756/nice/Sigma%20-%20sd%20Quattro%20-%203%3A2.X3F' },
    @{ Name = 'canon-eos-20d.cr2'; Sha256 = 'db78660d48589130d99e0bc621424851eb0f84df052634fb5bc89c7a02fe7033'; Url = 'https://raw.pixls.us/getfile.php/1018/nice/Canon%20-%20EOS%2020D%20-%20RAW%20%283%3A2%29.CR2' },
    @{ Name = 'canon-eos-40d-sraw.cr2'; Sha256 = 'ba644e7dd2abe74eca260e67f0206ff113bf0f62e710f8130611e964d6be5bf1'; Url = 'https://raw.pixls.us/getfile.php/2102/nice/Canon%20-%20EOS%2040D%20-%20sRAW2%20%28sRAW%29%20%283%3A2%29.CR2' },
    @{ Name = 'canon-eos-kiss-digital-n-rotated.cr2'; Sha256 = '49c78e5c36255cdd3f9fd219e86e3325bfa67be4d8d1ab5e21c21c3fd53c294e'; Url = 'https://raw.pixls.us/getfile.php/1185/nice/Canon%20-%20EOS%20Kiss%20Digital%20N%20-%20RAW%20%283%3A2%29.CR2' },
    @{ Name = 'nikon-d2h.nef'; Sha256 = '155edb938f884ea7372ce98d4ff5f965c3e413b43b95bc9923da6e92082cf914'; Url = 'https://raw.pixls.us/getfile.php/5227/nice/Nikon%20-%20D2H%20-%2012bit%2012bit%20compressed%20%28Lossy%20%28type%201%29%29%20%283%3A2%29.NEF' },
    @{ Name = 'nikon-d70s.nef'; Sha256 = '6c46f9854033e7623ebbb5de8c951df6762ca44f05d45035ccc5cb7e9f4e5692'; Url = 'https://raw.pixls.us/getfile.php/1117/nice/Nikon%20-%20D70s%20-%2012bit%2012bit%20compressed%20%28Lossy%20%28type%201%29%29%20%283%3A2%29.NEF' },
    @{ Name = 'nikon-d3.nef'; Sha256 = '880c60c5f611adf6b70de3d099f8433492de3a3d96866d570122f17f0a651fc8'; Url = 'https://raw.pixls.us/getfile.php/3516/nice/Nikon%20-%20D3%20-%2012bit%2012bit%20compressed%20%28Lossy%20%28type%202%29%29%20%283%3A2%29.NEF' },
    @{ Name = 'nikon-df-rotated.nef'; Sha256 = '902483437706df59bed669ebb4eb7d9ecaf95b23a70d6c79e319316ce50ee884'; Url = 'https://raw.pixls.us/getfile.php/1386/nice/Nikon%20-%20Df%20-%2012bit%2012bit%20compressed%20%28Lossy%20%28type%202%29%29%20%283%3A2%29.NEF' },
    @{ Name = 'sony-a100.arw'; Sha256 = '4df7981ac4baec9648b4f106751ea4b09a6e27bc70a15f8ef09bec051fec362d'; Url = 'https://raw.pixls.us/getfile.php/2346/nice/Sony%20-%20DSLR-A100%20-%20compressed%20%283%3A2%29.ARW' },
    @{ Name = 'sony-ilce-7s.arw'; Sha256 = 'a35ebb2fbec929daa5beb20d1ce5c15a8aac7b1a7a231455387f3df8a7442e07'; Url = 'https://raw.pixls.us/getfile.php/1582/nice/Sony%20-%20ILCE-7S%20-%2014bit%2014bit%20compressed%20%283%3A2%29.ARW' },
    @{ Name = 'sony-a450-rotated.arw'; Sha256 = '7c2205a14fbfb05aa22e2e290750ea8894229a52a106a67516b4e797759e37ad'; Url = 'https://raw.pixls.us/getfile.php/2349/nice/Sony%20-%20DSLR-A450%20-%2012bit%2012bit%20compressed%20%283%3A2%29.ARW' },
    @{ Name = 'olympus-e10.orf'; Sha256 = '2bfdade72439017a60a47aad1e5bbcb1aca36f2be7a21e94a4257a678fb6f4da'; Url = 'https://raw.pixls.us/getfile.php/5424/nice/Olympus%20-%20E-10%20-%2016bit%20%284%3A3%29.ORF' },
    @{ Name = 'olympus-e3.orf'; Sha256 = 'ba4605bbd9fe9b77971da444bcdb1698904c0ad4ebdf9dd42dd45ab255f3f6d4'; Url = 'https://raw.pixls.us/getfile.php/1803/nice/Olympus%20-%20E-3%20-%2016bit%20%284%3A3%29.ORF' },
    @{ Name = 'olympus-em5.orf'; Sha256 = 'b2221977cbc6eba0d3b363351072092e5647eaa0d02a5d8432b5d985b29f669f'; Url = 'https://raw.pixls.us/getfile.php/1084/nice/Olympus%20-%20E-M5%20-%2016bit%20%284%3A3%29.ORF' },
    @{ Name = 'panasonic-lx7.rw2'; Sha256 = '18dfc3877e944020299635a8c2fad61fca2485b1c9b67645a495ee870a86e599'; Url = 'https://raw.pixls.us/getfile.php/7010/nice/Panasonic%20-%20DMC-LX7%20-%203%3A2.RW2' },
    @{ Name = 'panasonic-lx3.rw2'; Sha256 = '6663d5b6539de15640ded2b0f422c8affb8cf1bec93e27b3548e970226c063a5'; Url = 'https://raw.pixls.us/getfile.php/1395/nice/Panasonic%20-%20DMC-LX3%20-%201%3A1.RW2' },
    @{ Name = 'panasonic-gh5.rw2'; Sha256 = '464b0cfc9c8960d632aba83f8b2f6d021c8e9bece1d18902e3b64eb2f1c6b0cf'; Url = 'https://raw.pixls.us/getfile.php/1514/nice/Panasonic%20-%20DC-GH5%20-%201%3A1.RW2' },
    @{ Name = 'pentax-k10d.pef'; Sha256 = 'e35ae4154a468be3154f5f462e884ba5941f010d3e8f23d347fbec14809f44d3'; Url = 'https://raw.pixls.us/getfile.php/2239/nice/Pentax%20-%20K10D%20-%2012bit%2012bit%20compressed%20%283%3A2%29.PEF' },
    @{ Name = 'fujifilm-s5000.raf'; Sha256 = 'dabd5e74521a6980156be9fd4b88d0c37b0fe4d0e0e6f5c12db8cffff1b76297'; Url = 'https://raw.pixls.us/getfile.php/2726/nice/Fujifilm%20-%20FinePix%20S5000%20-%204%3A3.RAF' },
    @{ Name = 'minolta-alpha-sweet-digital.mrw'; Sha256 = 'cad52fa1585f374ad7710d353295a7b8e3d9e148ad441d67d492010ef32dc5a5'; Url = 'https://raw.pixls.us/getfile.php/8070/nice/Minolta%20-%20Alpha%20Sweet%20Digital%20-%203%3A2.MRW' },
    @{ Name = 'adobe-dng-converter-canon-5d3.dng'; Sha256 = 'b22f1e36331f679abb8b13e433b9bbde3988723adf10fee7aa0dda6381016a98'; Url = 'https://raw.pixls.us/getfile.php/1033/nice/Adobe%20DNG%20Converter%20-%20Canon%20EOS%205D%20Mark%20III%20-%20Lossy%20JPEG%20compression%2C%20rgb%20%283%3A2%29.DNG' }
)

New-Item -ItemType Directory -Force -Path $Dest | Out-Null
$failed = 0
foreach ($s in $samples) {
    $path = Join-Path $Dest $s.Name
    if ((Test-Path -LiteralPath $path) -and ((Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash -eq $s.Sha256)) {
        Write-Host ('ok       {0}' -f $s.Name)
        continue
    }
    Write-Host ('fetching {0}' -f $s.Name)
    Invoke-WebRequest -Uri $s.Url -OutFile $path -UseBasicParsing
    $actual = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash
    if ($actual -ne $s.Sha256) {
        Remove-Item -LiteralPath $path -Force
        Write-Error ('{0}: SHA-256 mismatch (expected {1}, got {2})' -f $s.Name, $s.Sha256, $actual) -ErrorAction Continue
        $failed++
    }
}
if ($failed -gt 0) { throw ('{0} sample(s) failed verification' -f $failed) }
Write-Host ('{0} samples in {1}' -f $samples.Count, $Dest)
