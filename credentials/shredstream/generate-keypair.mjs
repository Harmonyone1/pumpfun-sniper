/**
 * Generate a Solana ed25519 keypair for ShredStream authentication
 *
 * This keypair is ONLY for ShredStream authentication (challenge-response).
 * It should have NO FUNDS and be separate from any trading keypairs.
 *
 * Usage: node generate-keypair.mjs
 */

import { generateKeyPairSync, randomBytes } from 'crypto';
import { writeFileSync, existsSync } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Base58 alphabet (Bitcoin/Solana style)
const BASE58_ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz';

function toBase58(buffer) {
    const bytes = [...buffer];
    const digits = [0];

    for (let i = 0; i < bytes.length; i++) {
        let carry = bytes[i];
        for (let j = 0; j < digits.length; j++) {
            carry += digits[j] << 8;
            digits[j] = carry % 58;
            carry = Math.floor(carry / 58);
        }
        while (carry > 0) {
            digits.push(carry % 58);
            carry = Math.floor(carry / 58);
        }
    }

    // Handle leading zeros
    let output = '';
    for (let i = 0; i < bytes.length && bytes[i] === 0; i++) {
        output += BASE58_ALPHABET[0];
    }

    for (let i = digits.length - 1; i >= 0; i--) {
        output += BASE58_ALPHABET[digits[i]];
    }

    return output;
}

function generateSolanaKeypair() {
    // Generate ed25519 keypair
    const { publicKey, privateKey } = generateKeyPairSync('ed25519');

    // Export keys in raw format
    const publicKeyRaw = publicKey.export({ type: 'spki', format: 'der' });
    const privateKeyRaw = privateKey.export({ type: 'pkcs8', format: 'der' });

    // Extract the actual key bytes
    // SPKI format for ed25519: 12 byte header + 32 byte public key
    const publicKeyBytes = publicKeyRaw.slice(-32);

    // PKCS8 format for ed25519: 16 byte header + 32 byte private key + optional public
    // The private key is at offset 16, length 32
    const privateKeyBytes = privateKeyRaw.slice(16, 48);

    // Solana keypair format: 64 bytes = 32 byte private + 32 byte public
    const keypairBytes = Buffer.concat([privateKeyBytes, publicKeyBytes]);

    return {
        publicKey: publicKeyBytes,
        privateKey: privateKeyBytes,
        keypairBytes: keypairBytes,
        publicKeyBase58: toBase58(publicKeyBytes)
    };
}

function main() {
    console.log('========================================');
    console.log('  ShredStream Authentication Keypair');
    console.log('========================================\n');

    const keypairPath = join(__dirname, 'shredstream-auth-keypair.json');

    // Check if keypair already exists
    if (existsSync(keypairPath)) {
        console.log('WARNING: Keypair already exists at:');
        console.log(`  ${keypairPath}\n`);
        console.log('To generate a new one, delete the existing file first.');

        // Read and display existing public key
        const existing = JSON.parse(require('fs').readFileSync(keypairPath, 'utf8'));
        const pubkey = toBase58(Buffer.from(existing.slice(32)));
        console.log(`\nExisting Public Key: ${pubkey}`);
        return;
    }

    // Generate new keypair
    const keypair = generateSolanaKeypair();

    // Save in Solana CLI format (array of 64 bytes)
    const keypairArray = [...keypair.keypairBytes];
    writeFileSync(keypairPath, JSON.stringify(keypairArray), 'utf8');

    console.log('Generated new ed25519 keypair for ShredStream authentication.\n');
    console.log('PUBLIC KEY (submit this to ShredStream):');
    console.log('─'.repeat(50));
    console.log(`  ${keypair.publicKeyBase58}`);
    console.log('─'.repeat(50));
    console.log('\nKeypair saved to:');
    console.log(`  ${keypairPath}\n`);

    console.log('IMPORTANT REMINDERS:');
    console.log('  • This keypair is ONLY for ShredStream authentication');
    console.log('  • Do NOT send any funds to this address');
    console.log('  • Keep the keypair file secure');
    console.log('  • Do NOT commit this file to version control');
    console.log('\nNext steps:');
    console.log('  1. Copy the public key above');
    console.log('  2. Submit it to the ShredStream approval form');
    console.log('  3. Wait for approval (usually 24-48 hours)');
    console.log('  4. Configure the keypair path in your .env file');
}

main();
