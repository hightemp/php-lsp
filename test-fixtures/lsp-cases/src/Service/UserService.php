<?php

declare(strict_types=1);

namespace App\Service;

use App\Model\AdminUser;
use App\Model\User;

// CASE: Valid service used by multiple LSP feature fixtures.
class UserService
{
    /** @var User[] */
    private array $users = [];

    // CASE: Parameter type hint and method reference.
    public function add(User $user): void
    {
        $this->users[] = $user;
    }

    // CASE: Return type hint and class construction.
    public function create(string $name): User
    {
        return new User($name);
    }

    // CASE: Cross-file type + method call reference.
    public function createAdmin(string $name): AdminUser
    {
        $admin = new AdminUser($name);
        $admin->getName();
        return $admin;
    }
}

